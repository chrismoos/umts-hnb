// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn receive_on(receiver: &mut AmEntity, bytes: &[u8], tick: u64, path: ReceiveProvenance) {
    receiver
        .receive_prepared_timed(
            ReceiveTime::simultaneous(at(tick)).with_provenance(path),
            bytes,
            |_| {
                Ok::<_, ()>(PreparedReceive {
                    plaintext: bytes,
                    reset_counters: None,
                })
            },
        )
        .unwrap();
}

#[test]
fn partial_successor_sdu_does_not_retain_an_unrelated_retired_path() {
    let old = NonZeroU64::new(1).unwrap();
    let current = NonZeroU64::new(2).unwrap();
    for provenance in [
        ReceiveProvenance::Path(old),
        ReceiveProvenance::Path(current),
        ReceiveProvenance::Unspecified,
        ReceiveProvenance::Mixed,
    ] {
        let mut sender = entity(16);
        let mut receiver = entity(16);
        sender.enqueue(&[0x42; 40]).unwrap();
        let (_, bytes) = send(&mut sender, 0).unwrap();
        receive_on(&mut receiver, &bytes, 0, provenance);
        assert!(receiver.pending_receive_path(old));
        receiver
            .deliver_with(|delivery| {
                assert_eq!(delivery.count(), 0);
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
        assert!(receiver.pending_receive_data());
        assert_eq!(
            receiver.pending_receive_path(old),
            provenance != ReceiveProvenance::Path(current)
        );
        assert_eq!(
            receiver.pending_receive_path(current),
            provenance != ReceiveProvenance::Path(old)
        );
    }
}

#[test]
fn reordered_successor_pdus_conservatively_retain_old_path() {
    let old = NonZeroU64::new(1).unwrap();
    let current = NonZeroU64::new(2).unwrap();
    let mut sender = entity(16);
    let mut receiver = entity(16);
    sender.enqueue(&[0x42; 40]).unwrap();
    let _first = send(&mut sender, 0).unwrap();
    let (_, second) = send(&mut sender, 1).unwrap();
    receive_on(&mut receiver, &second, 1, ReceiveProvenance::Path(current));
    assert!(receiver.pending_receive_path(old));
    assert_eq!(receiver.deliver_with(|_| Ok::<_, ()>(())), Ok(None));
    assert!(receiver.pending_receive_path(old));
}

#[test]
fn earliest_observed_air_start_survives_fragment_reordering_and_failed_delivery() {
    for missing in [None, Some(0), Some(1), Some(2)] {
        let mut sender = entity(16);
        let mut receiver = entity(16);
        sender.enqueue(&[0x5a; 40]).unwrap();
        let pdus = (0..3)
            .map(|tick| send(&mut sender, tick).unwrap().1)
            .collect::<Vec<_>>();
        for (tick, index) in [1, 0, 2].into_iter().enumerate() {
            let time = ReceiveTime::simultaneous(at(100 + tick as u64));
            let time = if missing == Some(index) {
                time
            } else {
                time.with_air_start(at(10 + index as u64)).unwrap()
            };
            receiver
                .receive_prepared_timed(time, &pdus[index], |_| {
                    Ok::<_, ()>(PreparedReceive {
                        plaintext: &pdus[index],
                        reset_counters: None,
                    })
                })
                .unwrap();
        }
        let duplicate = ReceiveTime::simultaneous(at(104))
            .with_air_start(at(0))
            .unwrap();
        receiver
            .receive_prepared_timed(duplicate, &pdus[1], |_| {
                Ok::<_, ()>(PreparedReceive {
                    plaintext: &pdus[1],
                    reset_counters: None,
                })
            })
            .unwrap();
        for _ in 0..2 {
            receiver
                .deliver_with(|delivery| {
                    assert_eq!(delivery.count(), 0);
                    Ok::<_, ()>(())
                })
                .unwrap()
                .unwrap();
        }
        let expected = missing.is_none().then_some(at(10));
        assert_eq!(
            receiver.deliver_with(|delivery| {
                assert_eq!(
                    delivery.covered_sdus().next().unwrap().1.air_start(),
                    expected
                );
                Err::<(), _>(7)
            }),
            Err(PublishError::Downstream(7))
        );
        receiver
            .deliver_with(|delivery| {
                let (bytes, coverage) = delivery.covered_sdus().next().unwrap();
                assert_eq!(bytes, &[0x5a; 40]);
                assert_eq!(coverage.air_start(), expected);
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
    }
}

#[test]
fn air_start_must_precede_reception_in_the_same_epoch_and_survive_service_delay() {
    let time = ReceiveTime::new(at(10), at(20)).unwrap();
    assert_eq!(time.with_air_start(at(11)), Err(AmError::Time));
    let mut foreign = at(1);
    foreign.epoch = RadioEpoch::new(NonZeroU64::new(2).unwrap());
    assert_eq!(time.with_air_start(foreign), Err(AmError::Time));
    assert_eq!(
        time.with_air_start(at(3))
            .unwrap()
            .serviced_at(at(30))
            .unwrap()
            .air_start(),
        Some(at(3))
    );
}

#[test]
fn physical_origin_covers_all_fragments_and_cannot_be_upgraded_by_duplicates() {
    let old = ReceiveProvenance::Path(NonZeroU64::MIN);
    let new = ReceiveProvenance::Path(NonZeroU64::new(2).unwrap());
    for paths in [
        [new, new, new],
        [old, new, new],
        [new, old, new],
        [new, new, old],
        [ReceiveProvenance::Unspecified, new, new],
    ] {
        let mut sender = entity(16);
        let mut receiver = entity(16);
        sender.enqueue(&[0x5a; 40]).unwrap();
        let pdus = (0..3)
            .map(|tick| send(&mut sender, tick).unwrap().1)
            .collect::<Vec<_>>();
        for (tick, index) in [1, 0, 2].into_iter().enumerate() {
            receive_on(&mut receiver, &pdus[index], tick as u64, paths[index]);
        }
        receive_on(&mut receiver, &pdus[1], 4, new);
        let expected = if paths == [new; 3] {
            new
        } else {
            ReceiveProvenance::Mixed
        };
        for _ in 0..2 {
            receiver
                .deliver_with(|delivery| {
                    assert_eq!(delivery.count(), 0);
                    Ok::<_, ()>(())
                })
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            receiver.deliver_with(|delivery| {
                let (data, coverage) = delivery.covered_sdus().next().unwrap();
                assert_eq!(data, &[0x5a; 40]);
                assert_eq!(coverage.provenance(), expected);
                Err::<(), _>(9)
            }),
            Err(PublishError::Downstream(9))
        );
        receive_on(&mut receiver, &pdus[0], 5, new);
        receiver
            .deliver_with(|delivery| {
                assert_eq!(
                    delivery.covered_sdus().next().unwrap().1.provenance(),
                    expected
                );
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
    }
}

#[test]
fn deferred_delimiter_origin_is_required_and_next_sdu_starts_its_own_origin() {
    let old = ReceiveProvenance::Path(NonZeroU64::MIN);
    let new = ReceiveProvenance::Path(NonZeroU64::new(2).unwrap());
    for (pdu_bytes, sdu_bytes) in [(16, 14), (160, 157)] {
        let mut sender = entity(pdu_bytes);
        let mut receiver = entity(pdu_bytes);
        sender.enqueue(&vec![0x5a; sdu_bytes]).unwrap();
        let first = send(&mut sender, 0).unwrap().1;
        receive_on(&mut receiver, &first, 0, old);
        assert!(drain(&mut receiver).is_empty());
        sender.enqueue(b"next").unwrap();
        let delimiter = send(&mut sender, 1).unwrap().1;
        receive_on(&mut receiver, &delimiter, 1, new);
        receiver
            .deliver_with(|delivery| {
                let mut sdus = delivery.covered_sdus();
                let (data, coverage) = sdus.next().unwrap();
                assert_eq!(data, vec![0x5a; sdu_bytes]);
                assert_eq!(coverage.provenance(), ReceiveProvenance::Mixed);
                let (data, coverage) = sdus.next().unwrap();
                assert_eq!(data, b"next");
                assert_eq!(coverage.provenance(), new);
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
    }
}

#[test]
fn reset_discards_partial_origin_and_service_retiming_preserves_it() {
    let old = ReceiveProvenance::Path(NonZeroU64::MIN);
    let new = ReceiveProvenance::Path(NonZeroU64::new(2).unwrap());
    let mut sender = entity(16);
    let mut receiver = entity(16);
    sender.enqueue(&[0x5a; 40]).unwrap();
    receive_on(&mut receiver, &send(&mut sender, 0).unwrap().1, 0, old);
    assert!(drain(&mut receiver).is_empty());
    receiver
        .receive(at(1), &reset(16, false, false, 0))
        .unwrap();
    let mut fresh = entity(16);
    fresh.enqueue(b"fresh").unwrap();
    receive_on(&mut receiver, &send(&mut fresh, 2).unwrap().1, 2, new);
    receiver
        .deliver_with(|delivery| {
            let (bytes, coverage) = delivery.covered_sdus().next().unwrap();
            assert_eq!(bytes, b"fresh");
            assert_eq!(coverage.provenance(), new);
            Ok::<_, ()>(())
        })
        .unwrap()
        .unwrap();
    let time = ReceiveTime::new(at(10), at(20))
        .unwrap()
        .with_provenance(new);
    assert_eq!(time.serviced_at(at(19)), Err(AmError::Time));
    let later = time.serviced_at(at(30)).unwrap();
    assert_eq!(later.received(), at(10));
    assert_eq!(later.provenance(), new);
}

#[test]
fn delayed_receive_preserves_original_provenance_without_rewinding_service_timers() {
    let mut sender = entity(16);
    let mut receiver = entity(16);
    sender.enqueue(&[0x42; 40]).unwrap();
    let mut pdus = Vec::new();
    for tick in 0..3 {
        pdus.push(send(&mut sender, tick).unwrap().1);
    }
    receiver.tick(at(100)).unwrap();
    for (index, observed, serviced) in [(1, 10, 110), (0, 20, 112), (2, 30, 113)] {
        let service = ReceiveTime::new(at(observed), at(serviced)).unwrap();
        let bytes = &pdus[index];
        receiver
            .receive_prepared_timed(service, bytes, |_| {
                Ok::<_, ()>(PreparedReceive {
                    plaintext: bytes,
                    reset_counters: None,
                })
            })
            .unwrap();
    }
    let mut delivered = false;
    for _ in 0..3 {
        receiver
            .deliver_with(|delivery| {
                if let Some((bytes, coverage)) = delivery.covered_sdus().next() {
                    assert_eq!(bytes, &[0x42; 40]);
                    assert_eq!(coverage.first_received(), at(10));
                    delivered = true;
                }
                Ok::<_, ()>(())
            })
            .unwrap();
    }
    assert!(delivered);
    assert_eq!(receiver.tick(at(112)), Err(AmError::Time));
    assert_eq!(ReceiveTime::new(at(2), at(1)), Err(AmError::Time));
    let mut foreign = at(20);
    foreign.epoch = RadioEpoch::new(NonZeroU64::new(2).unwrap());
    assert_eq!(ReceiveTime::new(foreign, at(100)), Err(AmError::Time));
}

#[test]
fn receive_provenance_survives_reordering_duplicates_and_delayed_upper_admission() {
    let mut sender = entity(16);
    let mut receiver = entity(16);
    sender.enqueue(&[0x42; 40]).unwrap();
    let mut pdus = Vec::new();
    for tick in 0..8 {
        if !sender.pending_new_data() {
            break;
        }
        pdus.push(send(&mut sender, tick).unwrap().1);
    }
    assert_eq!(pdus.len(), 3);
    receiver.receive(at(10), &pdus[1]).unwrap();
    receiver.receive(at(20), &pdus[1]).unwrap();
    receiver.receive(at(30), &pdus[0]).unwrap();
    for _ in 0..2 {
        receiver
            .deliver_with(|delivery| {
                assert_eq!(delivery.sdus().count(), 0);
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
    }
    receiver.receive(at(40), &pdus[2]).unwrap();
    let mut rejected = None;
    assert_eq!(
        receiver.deliver_with(|delivery| {
            let coverage = delivery.covered_sdus().next().unwrap().1;
            assert_eq!(coverage.first_received(), at(10));
            rejected = Some(coverage);
            Err::<(), _>(7)
        }),
        Err(PublishError::Downstream(7))
    );
    receiver.receive(at(50), &pdus[2]).unwrap();
    let coverage = receiver
        .deliver_with(|delivery| Ok::<_, ()>(delivery.covered_sdus().next().unwrap().1))
        .unwrap()
        .unwrap();
    assert_eq!(Some(coverage), rejected);
    assert_eq!(coverage.first_received(), at(10));
}

#[test]
fn deferred_delimiters_are_part_of_sdu_coverage_and_confirmation() {
    for (pdu_bytes, sdu_bytes) in [(16, 14), (160, 157)] {
        let mut sender = entity(pdu_bytes);
        let mut receiver = entity(pdu_bytes);
        let payload = vec![0x5a; sdu_bytes];
        let receipt = sender.enqueue_tracked(&payload).unwrap();
        assert_eq!(sender.transmission(receipt), Ok(SduTransmission::Queued));
        let (_, first) = send(&mut sender, 0).unwrap();
        assert_eq!(
            sender.transmission(receipt),
            Ok(SduTransmission::InFlight {
                first: 0,
                last: 0,
                delimited: false
            })
        );
        receiver.receive(at(0), &first).unwrap();
        assert!(drain(&mut receiver).is_empty());
        sender.receive(at(1), &status(pdu_bytes, 1, &[])).unwrap();
        assert!(!matches!(
            sender.transmission(receipt),
            Ok(SduTransmission::Acknowledged)
        ));
        assert_eq!(
            sender.publish_next(at(2), |_| Err::<(), _>(7)),
            Err(PublishError::Downstream(7))
        );
        assert_eq!(
            sender.transmission(receipt),
            Ok(SduTransmission::InFlight {
                first: 0,
                last: 0,
                delimited: false
            })
        );
        let (_, delimiter) = send(&mut sender, 2).unwrap();
        assert_eq!(
            sender.transmission(receipt),
            Ok(SduTransmission::InFlight {
                first: 0,
                last: 1,
                delimited: true
            })
        );
        receiver.receive(at(2), &delimiter).unwrap();
        let mut rejected = None;
        assert_eq!(
            receiver.deliver_with(|delivery| {
                let (bytes, coverage) = delivery.covered_sdus().next().unwrap();
                assert_eq!(bytes, payload);
                assert_eq!(
                    (coverage.first_position(), coverage.last_position()),
                    (0, 1)
                );
                rejected = Some(coverage);
                Err::<(), _>(9)
            }),
            Err(PublishError::Downstream(9))
        );
        let coverage = receiver
            .deliver_with(|delivery| {
                let (bytes, coverage) = delivery.covered_sdus().next().unwrap();
                assert_eq!(bytes, payload);
                assert_eq!(Some(coverage), rejected);
                Ok::<_, ()>(coverage)
            })
            .unwrap()
            .unwrap();
        assert_eq!(receiver.published_status(), None);
        assert_eq!(
            receiver.publish_next(at(3), |_| Err::<(), _>(7)),
            Err(PublishError::Downstream(7))
        );
        assert_eq!(receiver.published_status(), None);
        let (_, report) = receiver
            .publish_next(at(3), |publication| {
                assert_eq!(publication.kind, PublicationKind::Status);
                assert!(publication.status_coverage.unwrap().covers(coverage));
                Ok::<_, ()>(publication.bytes.to_vec())
            })
            .unwrap()
            .unwrap();
        assert!(receiver.published_status().unwrap().covers(coverage));
        sender.receive(at(3), &report).unwrap();
        assert_eq!(
            sender.transmission(receipt),
            Ok(SduTransmission::Acknowledged)
        );
    }
}

#[test]
fn rejected_first_publication_and_stale_receipts_cannot_consume_tracking_state() {
    let mut sender = entity(16);
    let receipt = sender.enqueue_tracked(b"one").unwrap();
    assert_eq!(
        sender.publish_next(at(0), |_| Err::<(), _>(7)),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(sender.transmission(receipt), Ok(SduTransmission::Queued));
    assert_eq!(sender.release_receipt(receipt), Err(AmError::State));
    send(&mut sender, 0).unwrap();
    sender.receive(at(1), &status(16, 1, &[])).unwrap();
    sender.release_receipt(receipt).unwrap();
    assert_eq!(sender.transmission(receipt), Err(AmError::State));
    let next = sender.enqueue_tracked(b"two").unwrap();
    assert_ne!(next, receipt);
    assert_eq!(sender.release_receipt(receipt), Err(AmError::State));
    assert_eq!(sender.transmission(next), Ok(SduTransmission::Queued));
    let mut other = entity(16);
    let foreign = other.enqueue_tracked(b"other").unwrap();
    assert_eq!(sender.transmission(foreign), Err(AmError::State));
    assert_eq!(other.transmission(next), Err(AmError::State));
}

#[test]
fn concatenated_sdus_share_pdu_coverage_but_have_independent_receipts() {
    let mut sender = entity(16);
    let mut receiver = entity(16);
    let payloads: [&[u8]; 3] = [b"a", b"bb", b"ccc"];
    let receipts = payloads.map(|payload| sender.enqueue_tracked(payload).unwrap());
    let (_, bytes) = send(&mut sender, 0).unwrap();
    receiver.receive(at(0), &bytes).unwrap();
    receiver
        .deliver_with(|delivery| {
            let sdus = delivery.covered_sdus().collect::<Vec<_>>();
            assert_eq!(sdus.len(), payloads.len());
            for ((bytes, coverage), expected) in sdus.into_iter().zip(payloads) {
                assert_eq!(bytes, expected);
                assert_eq!(
                    (coverage.first_position(), coverage.last_position()),
                    (0, 0)
                );
            }
            Ok::<_, ()>(())
        })
        .unwrap()
        .unwrap();
    for receipt in receipts {
        assert_eq!(
            sender.transmission(receipt),
            Ok(SduTransmission::InFlight {
                first: 0,
                last: 0,
                delimited: true
            })
        );
    }
    sender.receive(at(1), &status(16, 1, &[])).unwrap();
    for receipt in receipts {
        assert_eq!(
            sender.transmission(receipt),
            Ok(SduTransmission::Acknowledged)
        );
    }
    sender.release_receipt(receipts[1]).unwrap();
    assert_eq!(
        sender.transmission(receipts[0]),
        Ok(SduTransmission::Acknowledged)
    );
    assert_eq!(
        sender.transmission(receipts[2]),
        Ok(SduTransmission::Acknowledged)
    );
}

#[test]
fn selective_ack_can_confirm_one_sdu_while_an_earlier_sdu_needs_retransmission() {
    let mut sender = entity(16);
    let first = sender.enqueue_tracked(b"first").unwrap();
    let (_, original) = send(&mut sender, 0).unwrap();
    let second = sender.enqueue_tracked(b"second").unwrap();
    send(&mut sender, 1).unwrap();
    sender.receive(at(2), &status(16, 2, &[0])).unwrap();
    assert_eq!(
        sender.transmission(first),
        Ok(SduTransmission::InFlight {
            first: 0,
            last: 0,
            delimited: true
        })
    );
    assert_eq!(
        sender.transmission(second),
        Ok(SduTransmission::Acknowledged)
    );
    let (kind, retransmitted) = send(&mut sender, 3).unwrap();
    assert!(matches!(
        kind,
        PublicationKind::Data {
            retransmission: true,
            sequence: 0,
            ..
        }
    ));
    assert_eq!(&retransmitted[2..], &original[2..]);
    sender.receive(at(4), &status(16, 2, &[])).unwrap();
    assert_eq!(
        sender.transmission(first),
        Ok(SduTransmission::Acknowledged)
    );
}

#[test]
fn out_of_order_holes_and_failed_status_publication_do_not_forge_coverage() {
    let mut sender = entity(16);
    let mut receiver = entity(16);
    let receipt = sender.enqueue_tracked(&[0x42; 40]).unwrap();
    let packets = (0..3)
        .map(|now| send(&mut sender, now).unwrap().1)
        .collect::<Vec<_>>();
    receiver.receive(at(0), &packets[2]).unwrap();
    send(&mut receiver, 0).unwrap();
    let incomplete = receiver.published_status().unwrap();
    assert_eq!(incomplete.next_position(), 0);
    receiver.receive(at(1), &packets[0]).unwrap();
    receiver.receive(at(2), &packets[1]).unwrap();
    let mut coverage = None;
    for _ in 0..3 {
        receiver
            .deliver_with(|delivery| {
                for (bytes, covered) in delivery.covered_sdus() {
                    assert_eq!(bytes, &[0x42; 40]);
                    assert_eq!((covered.first_position(), covered.last_position()), (0, 2));
                    coverage = Some(covered);
                }
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
    }
    let coverage = coverage.unwrap();
    assert!(!incomplete.covers(coverage));
    assert_eq!(
        receiver.publish_next(at(200), |_| Err::<(), _>(7)),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(receiver.published_status(), Some(incomplete));
    let (_, report) = send(&mut receiver, 200).unwrap();
    assert!(receiver.published_status().unwrap().covers(coverage));
    sender.receive(at(200), &report).unwrap();
    assert_eq!(
        sender.transmission(receipt),
        Ok(SduTransmission::Acknowledged)
    );
    let mut foreign = entity(16);
    for packet in &packets {
        foreign.receive(at(0), packet).unwrap();
    }
    send(&mut foreign, 0).unwrap();
    assert!(!foreign.published_status().unwrap().covers(coverage));
    receiver
        .receive(at(201), &reset(16, false, false, 0))
        .unwrap();
    assert_eq!(receiver.published_status(), None);
}

#[test]
fn reset_discards_started_receipts_but_preserves_queued_sdus_and_serials() {
    let mut sender = entity(16);
    let started = sender.enqueue_tracked(&[0x24; 40]).unwrap();
    let queued = sender.enqueue_tracked(b"queued").unwrap();
    send(&mut sender, 0).unwrap();
    sender.initiate_reset(at(1)).unwrap();
    send(&mut sender, 1).unwrap();
    sender.receive(at(2), &reset(16, false, true, 0)).unwrap();
    assert_eq!(sender.transmission(started), Ok(SduTransmission::Discarded));
    assert_eq!(sender.transmission(queued), Ok(SduTransmission::Queued));
    sender.release_receipt(started).unwrap();
    let (_, bytes) = send(&mut sender, 3).unwrap();
    assert_eq!(
        parse_amd(&bytes, LiWidth::Seven, false).unwrap().data,
        b"queued"
    );
    sender.receive(at(4), &status(16, 1, &[])).unwrap();
    assert_eq!(
        sender.transmission(queued),
        Ok(SduTransmission::Acknowledged)
    );
}

#[test]
fn retained_receipts_are_bounded_independently_of_the_sdu_queue() {
    let mut small = limits();
    small.queued_sdus = NonZeroUsize::MIN;
    let mut sender = AmEntity::new(config(16), small, at(0)).unwrap();
    let receipt = sender.enqueue_tracked(b"one").unwrap();
    send(&mut sender, 0).unwrap();
    sender.receive(at(1), &status(16, 1, &[])).unwrap();
    assert_eq!(sender.enqueue_tracked(b"two"), Err(AmError::Capacity));
    assert_eq!(
        sender.transmission(receipt),
        Ok(SduTransmission::Acknowledged)
    );
    sender.release_receipt(receipt).unwrap();
    let next = sender.enqueue_tracked(b"two").unwrap();
    assert_eq!(sender.transmission(next), Ok(SduTransmission::Queued));
    assert_eq!(sender.enqueue_tracked(b"three"), Err(AmError::Capacity));
    assert_eq!(sender.transmission(next), Ok(SduTransmission::Queued));
}
