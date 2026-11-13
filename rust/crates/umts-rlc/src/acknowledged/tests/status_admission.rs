// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn receive_work_includes_reordering_and_partial_sdu_history_not_just_undelivered_in_order_pdus() {
    let mut sender = entity(16);
    let mut receiver = entity(16);
    receiver.require_delivery_admission();
    let payload = [0x42; 40];
    sender.enqueue(&payload).unwrap();
    let mut pdus = Vec::new();
    for tick in 0..8 {
        if !sender.pending_new_data() {
            break;
        }
        pdus.push(send(&mut sender, tick).unwrap().1);
    }
    assert!(pdus.len() >= 3);
    assert!(!receiver.pending_receive_data());
    receiver.receive(at(0), &pdus[1]).unwrap();
    assert_eq!(receiver.status_deferral(), None);
    assert!(receiver.pending_receive_data());
    assert_eq!(receiver.deliver_with(|_| Ok::<_, ()>(())), Ok(None));
    receiver.receive(at(0), &pdus[0]).unwrap();
    for _ in 0..2 {
        receiver
            .deliver_with(|delivery| {
                assert_eq!(delivery.sdus().count(), 0);
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
    }
    assert_eq!(receiver.status_deferral(), None);
    assert!(receiver.pending_receive_data());
    let mut completed = false;
    for pdu in &pdus[2..] {
        receiver.receive(at(0), pdu).unwrap();
        assert!(receiver.pending_receive_data());
        receiver
            .deliver_with(|delivery| {
                for sdu in delivery.sdus() {
                    assert_eq!(sdu, payload);
                    completed = true;
                }
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
    }
    assert!(completed);
    assert!(!receiver.pending_receive_data());
}

#[test]
fn segmented_sdu_can_advance_a_small_window_without_acknowledging_upper_pending_completion() {
    let mut configuration = config(16);
    configuration.transmit_window = NonZeroU16::new(4).unwrap();
    configuration.receive_window = NonZeroU16::new(4).unwrap();
    configuration.status_prohibit_chips = None;
    let mut sender = AmEntity::new(configuration, limits(), at(0)).unwrap();
    let mut receiver = AmEntity::new(configuration, limits(), at(0)).unwrap();
    receiver.require_delivery_admission();
    let payload = [0x42; 120];
    let receipt = sender.enqueue_tracked(&payload).unwrap();
    let mut complete = None;
    let mut fragments = 0;
    for tick in 0..32 {
        let (_, bytes) =
            send(&mut sender, tick).expect("partial acknowledgments must keep the window moving");
        fragments += 1;
        receiver.receive(at(tick), &bytes).unwrap();
        assert_eq!(
            receiver.status_deferral(),
            Some(StatusDeferral::UndeliveredData)
        );
        assert_eq!(
            receiver.publish_next(at(tick), |_| -> Result<(), ()> {
                panic!("undelivered data escaped STATUS gate")
            }),
            Ok(None)
        );
        let mut delivered = false;
        receiver
            .deliver_with(|delivery| {
                for (bytes, coverage) in delivery.covered_sdus() {
                    assert_eq!(bytes, payload);
                    complete = Some(coverage);
                    delivered = true;
                }
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
        if delivered {
            receiver.upper_admission_pending(true);
            assert_eq!(
                receiver.status_deferral(),
                Some(StatusDeferral::UpperAdmission)
            );
            assert_eq!(
                receiver.publish_next(at(tick), |_| -> Result<(), ()> {
                    panic!("unadmitted completion was acknowledged")
                }),
                Ok(None)
            );
            assert!(!matches!(
                sender.transmission(receipt),
                Ok(SduTransmission::Acknowledged)
            ));
            receiver.upper_admission_pending(false);
        }
        let (kind, report) = send(&mut receiver, tick).unwrap();
        assert_eq!(kind, PublicationKind::Status);
        assert_eq!(
            parse_status(&report).unwrap().acknowledge,
            Some(receiver.receive_sequence())
        );
        sender.receive(at(tick), &report).unwrap();
        if delivered {
            break;
        }
    }
    assert!(fragments > usize::from(configuration.transmit_window.get()));
    assert_eq!(
        sender.transmission(receipt),
        Ok(SduTransmission::Acknowledged)
    );
    assert!(
        receiver
            .published_status()
            .unwrap()
            .covers(complete.unwrap())
    );
}

#[test]
fn deferred_status_does_not_block_forward_data_or_advance_status_prohibit() {
    let mut sender = entity(16);
    let mut receiver = entity(16);
    receiver.require_delivery_admission();
    sender.enqueue(b"uplink").unwrap();
    let (_, bytes) = send(&mut sender, 0).unwrap();
    receiver.receive(at(0), &bytes).unwrap();
    receiver.enqueue(b"downlink").unwrap();
    let (kind, _) = send(&mut receiver, 0).unwrap();
    assert!(matches!(kind, PublicationKind::Data { .. }));
    assert_eq!(receiver.published_status(), None);
    assert_eq!(
        receiver.deliver_with(|_| Err::<(), _>(7)),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(
        receiver.status_deferral(),
        Some(StatusDeferral::UndeliveredData)
    );
    assert_eq!(receiver.publish_next(at(1), |_| Ok::<_, ()>(())), Ok(None));
    receiver.deliver_with(|_| Ok::<_, ()>(())).unwrap().unwrap();
    receiver.upper_admission_pending(true);
    receiver.require_delivery_admission();
    assert_eq!(
        receiver.status_deferral(),
        Some(StatusDeferral::UpperAdmission)
    );
    assert_eq!(receiver.publish_next(at(2), |_| Ok::<_, ()>(())), Ok(None));
    receiver.upper_admission_pending(false);
    assert_eq!(
        receiver.publish_next(at(2), |_| Err::<(), _>(9)),
        Err(PublishError::Downstream(9))
    );
    assert_eq!(receiver.published_status(), None);
    let (kind, report) = send(&mut receiver, 2).unwrap();
    assert_eq!(kind, PublicationKind::Status);
    assert_eq!(
        parse_status(&report).unwrap().acknowledge,
        Some(receiver.receive_sequence())
    );
}

#[test]
fn reset_and_an_independent_entity_cannot_clear_an_upper_admission_hold() {
    let mut sender = entity(16);
    sender.enqueue(b"held").unwrap();
    let (_, bytes) = send(&mut sender, 0).unwrap();
    let mut held = entity(16);
    let mut peer = entity(16);
    for receiver in [&mut held, &mut peer] {
        receiver.require_delivery_admission();
        receiver.receive(at(0), &bytes).unwrap();
        receiver.deliver_with(|_| Ok::<_, ()>(())).unwrap().unwrap();
    }
    held.upper_admission_pending(true);
    assert_eq!(held.publish_next(at(0), |_| Ok::<_, ()>(())), Ok(None));
    assert_eq!(send(&mut peer, 0).unwrap().0, PublicationKind::Status);
    assert_eq!(held.status_deferral(), Some(StatusDeferral::UpperAdmission));
    held.receive(at(1), &reset(16, false, false, 0)).unwrap();
    assert_eq!(send(&mut held, 1).unwrap().0, PublicationKind::ResetAck);
    assert_eq!(held.status_deferral(), Some(StatusDeferral::UpperAdmission));
    assert_eq!(held.published_status(), None);
    held.require_delivery_admission();
    assert_eq!(held.status_deferral(), Some(StatusDeferral::UpperAdmission));
}
