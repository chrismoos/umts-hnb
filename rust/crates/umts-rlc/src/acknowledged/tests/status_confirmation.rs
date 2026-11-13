// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn completion(receiver: &mut AmEntity, peer: &mut AmEntity) -> ReceiveCoverage {
    peer.enqueue(b"complete").unwrap();
    let (_, bytes) = send(peer, 0).unwrap();
    receiver.receive(at(0), &bytes).unwrap();
    receiver
        .deliver_with(|delivery| Ok::<_, ()>(delivery.covered_sdus().next().unwrap().1))
        .unwrap()
        .unwrap()
}

#[test]
fn piggybacked_status_requires_peer_ack_and_preserves_the_rrc_payload() {
    for size in [30, 130] {
        let mut network = entity(size);
        let mut peer = entity(size);
        let completed = completion(&mut network, &mut peer);
        network.request_status_confirmation(completed).unwrap();
        let (_, standalone) = send(&mut network, 1).unwrap();
        peer.receive(at(1), &standalone).unwrap();
        assert!(network.published_status().unwrap().covers(completed));
        assert_eq!(network.acknowledged_status(), None);
        network.enqueue(b"release").unwrap();
        assert!(send(&mut network, 2).is_none());
        let before = network.window_positions();
        assert_eq!(
            network.publish_next(at(101), |_| Err::<(), _>(7)),
            Err(PublishError::Downstream(7))
        );
        assert_eq!(network.window_positions(), before);
        assert_eq!(network.acknowledged_status(), None);
        let (kind, data) = send(&mut network, 101).unwrap();
        assert!(matches!(
            kind,
            PublicationKind::Data {
                retransmission: false,
                ..
            }
        ));
        let (indicators, payload_start): (&[u8], usize) = if size == 30 {
            (&[0x0f, 0xfc], 4)
        } else {
            (&[0x00, 0x0f, 0xff, 0xfc], 6)
        };
        assert_eq!(&data[2..payload_start], indicators);
        assert_eq!(
            &data[payload_start + 7..payload_start + 10],
            &[0x02, 0x00, 0x10]
        );
        let decoded = parse_amd(&data, LiWidth::for_fixed_size(size), false).unwrap();
        assert_eq!(decoded.data, b"release");
        assert_eq!(
            parse_status(decoded.piggybacked_status.unwrap())
                .unwrap()
                .acknowledge,
            Some(1)
        );
        assert_eq!(network.acknowledged_status(), None);
        peer.receive(at(102), &data).unwrap();
        assert_eq!(drain(&mut peer), vec![b"release".to_vec()]);
        let (_, ack) = send(&mut peer, 103).unwrap();
        network.receive(at(103), &ack).unwrap();
        assert!(network.acknowledged_status().unwrap().covers(completed));
    }
}

#[test]
fn missing_piggyback_carrier_cannot_confirm_and_retransmission_keeps_coverage() {
    let mut network = entity(30);
    let mut peer = entity(30);
    let completed = completion(&mut network, &mut peer);
    network.request_status_confirmation(completed).unwrap();
    send(&mut network, 1).unwrap();
    network.enqueue(b"release").unwrap();
    let (_, first) = send(&mut network, 101).unwrap();
    network.receive(at(102), &status(30, 1, &[0])).unwrap();
    assert_eq!(network.acknowledged_status(), None);
    let (kind, repeat) = send(&mut network, 103).unwrap();
    assert!(matches!(
        kind,
        PublicationKind::Data {
            retransmission: true,
            ..
        }
    ));
    let first = parse_amd(&first, LiWidth::Seven, false).unwrap();
    let repeat = parse_amd(&repeat, LiWidth::Seven, false).unwrap();
    assert_eq!(first.data, repeat.data);
    assert_eq!(first.piggybacked_status, repeat.piggybacked_status);
    network.receive(at(104), &status(30, 1, &[])).unwrap();
    assert!(network.acknowledged_status().unwrap().covers(completed));
    network.initiate_reset(at(105)).unwrap();
    assert_eq!(
        network.request_status_confirmation(completed),
        Err(AmError::State)
    );
}

#[test]
fn full_data_and_foreign_input_do_not_invent_confirmation() {
    let mut network = entity(30);
    let mut peer = entity(30);
    let completed = completion(&mut network, &mut peer);
    network.request_status_confirmation(completed).unwrap();
    send(&mut network, 1).unwrap();
    network.enqueue(&[0x55; 100]).unwrap();
    let (_, data) = send(&mut network, 101).unwrap();
    assert!(
        parse_amd(&data, LiWidth::Seven, false)
            .unwrap()
            .piggybacked_status
            .is_none()
    );
    network.receive(at(102), &status(30, 1, &[])).unwrap();
    assert_eq!(network.acknowledged_status(), None);
    let foreign = completion(&mut entity(30), &mut entity(30));
    assert_eq!(
        network.request_status_confirmation(foreign),
        Err(AmError::State)
    );
}

#[test]
fn upper_admission_and_reset_keep_confirmation_bound_to_the_receive_generation() {
    let mut network = entity(30);
    let mut peer = entity(30);
    let completed = completion(&mut network, &mut peer);
    network.request_status_confirmation(completed).unwrap();
    send(&mut network, 1).unwrap();
    network.require_delivery_admission();
    network.upper_admission_pending(true);
    network.enqueue(b"release").unwrap();
    let (_, data) = send(&mut network, 101).unwrap();
    assert!(
        parse_amd(&data, LiWidth::Seven, false)
            .unwrap()
            .piggybacked_status
            .is_none()
    );
    network.upper_admission_pending(false);
    network.enqueue(b"next").unwrap();
    let (_, data) = send(&mut network, 102).unwrap();
    assert!(
        parse_amd(&data, LiWidth::Seven, false)
            .unwrap()
            .piggybacked_status
            .is_some()
    );
    network.receive(at(103), &status(30, 2, &[])).unwrap();
    assert!(network.acknowledged_status().unwrap().covers(completed));
    network.initiate_reset(at(104)).unwrap();
    assert_eq!(network.acknowledged_status(), None);
    let (_, reset) = send(&mut network, 105).unwrap();
    peer.receive(at(106), &reset).unwrap();
    let (_, reset_ack) = send(&mut peer, 107).unwrap();
    network.receive(at(108), &reset_ack).unwrap();
    assert_eq!(network.state(), AmState::Ready);
    assert_eq!(network.acknowledged_status(), None);
    assert_eq!(
        network.request_status_confirmation(completed),
        Err(AmError::State)
    );
}
