// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};
use umts_types::{RadioEpoch, time::ChipInstant};

mod activation_span;
mod delivery_forecast;
mod receipts;
mod reset_initialization;
mod status_admission;
mod status_confirmation;

#[test]
fn tiny_sdus_split_at_the_li_bound_without_losing_order() {
    let mut bound = limits();
    bound.queued_sdus = size(MAX_QUEUED_SDUS);
    let mut sender = AmEntity::new(config(1024), bound, at(0)).unwrap();
    let mut receiver = AmEntity::new(config(1024), bound, at(0)).unwrap();
    for value in 0..MAX_QUEUED_SDUS {
        sender.enqueue(&[value as u8]).unwrap();
    }
    let mut received = Vec::new();
    for now in 0..4 {
        let Some((_, bytes)) = send(&mut sender, now) else {
            break;
        };
        receiver.receive(at(now), &bytes).unwrap();
        receiver
            .deliver_with(|delivery| {
                received.extend(delivery.sdus().map(<[u8]>::to_vec));
                Ok::<_, ()>(())
            })
            .unwrap();
        if received.len() == MAX_QUEUED_SDUS {
            break;
        }
    }
    assert_eq!(
        received,
        (0..MAX_QUEUED_SDUS)
            .map(|value| vec![value as u8])
            .collect::<Vec<_>>()
    );
}

#[test]
fn malformed_wire_pdus_are_discarded_without_losing_following_data() {
    const RESERVED_STATUS_SUFI: u8 = 0x0f;
    const RESERVED_CONTROL_TYPE: u8 = 0x70;
    let mut sender = entity(16);
    sender.enqueue(b"survives").unwrap();
    let (_, valid) = send(&mut sender, 0).unwrap();
    let mut reserved_header = valid.clone();
    reserved_header[1] |= wire::HE_MASK as u8;
    let mut reserved_status = vec![0; valid.len()];
    reserved_status[0] = RESERVED_STATUS_SUFI;
    let mut reserved_control = vec![0; valid.len()];
    reserved_control[0] = RESERVED_CONTROL_TYPE;
    for invalid in [reserved_header, reserved_status, reserved_control] {
        let mut receiver = entity(16);
        assert_eq!(receiver.receive(at(0), &invalid), Ok(AmReceive::Ignored));
        assert_eq!(receiver.counters().malformed_pdus, 1);
        assert_eq!(receiver.receive_sequence(), 0);
        assert_eq!(receiver.receive(at(1), &valid), Ok(AmReceive::Stored));
        assert_eq!(drain(&mut receiver), vec![b"survives".to_vec()]);
    }
}

#[test]
fn malformed_deciphered_li_is_discarded_but_preparation_errors_remain_errors() {
    let mut receiver = entity(16);
    let mut bytes = vec![0xff; 16];
    wire::data_header(&mut bytes, 0, false, true);
    assert!(parse_amd(&bytes, LiWidth::Seven, false).is_err());
    assert_eq!(receiver.receive(at(0), &bytes), Ok(AmReceive::Ignored));
    assert_eq!(receiver.counters().malformed_pdus, 1);
    assert_eq!(
        receiver.receive_prepared(at(1), &bytes, |_| Err::<&[u8], _>(7)),
        Err(ReceiveError::Preparation(7))
    );
    assert_eq!(receiver.counters().malformed_pdus, 1);
}

fn explicit_one_short_pdu() -> Vec<u8> {
    let mut bytes = vec![0x51; 159];
    bytes[..4].copy_from_slice(&[0x80, 0x01, 0x01, 0x34]);
    bytes[158] = 0;
    bytes
}

fn following_one_short_pdu() -> Vec<u8> {
    let mut bytes = vec![0; 159];
    bytes[..8].copy_from_slice(&[0x80, 0x09, 0xff, 0xf7, 0x00, 0x15, 0xff, 0xfe]);
    bytes[8..18].fill(0x62);
    bytes
}

#[test]
fn repeated_one_short_after_explicit_padding_does_not_redeliver_or_commit_on_backpressure() {
    let mut receiver = entity(159);
    let first = explicit_one_short_pdu();
    assert!(
        parse_amd(&first, LiWidth::Fifteen, false)
            .unwrap()
            .one_byte_padding
    );
    receiver.receive(at(0), &first).unwrap();
    assert_eq!(drain(&mut receiver), vec![vec![0x51; 154]]);
    receiver.receive(at(1), &following_one_short_pdu()).unwrap();
    let before = receiver.delivery_evidence();
    assert!(matches!(
        receiver.deliver_with(|_| Err::<(), _>(7)),
        Err(PublishError::Downstream(7))
    ));
    assert_eq!(receiver.delivery_evidence(), before);
    assert_eq!(receiver.counters().repeated_one_short_markers, 0);
    assert_eq!(drain(&mut receiver), vec![vec![0x62; 10]]);
    assert_eq!(receiver.counters().delivered_sdus, 2);
    assert_eq!(receiver.counters().repeated_one_short_markers, 1);
    assert!(drain(&mut receiver).is_empty());
}

#[test]
fn undelimited_one_short_still_trims_and_delivers_its_real_fragment() {
    let mut receiver = entity(159);
    let mut first = vec![0x51; 159];
    first[..2].copy_from_slice(&[0x80, 0x00]);
    first[158] = 0;
    receiver.receive(at(0), &first).unwrap();
    assert!(drain(&mut receiver).is_empty());
    receiver.receive(at(1), &following_one_short_pdu()).unwrap();
    assert_eq!(receiver.delivery_evidence().unwrap().partial_bytes, 157);
    assert_eq!(drain(&mut receiver), vec![vec![0x51; 156], vec![0x62; 10]]);
    assert_eq!(receiver.counters().repeated_one_short_markers, 0);
}

#[test]
fn reset_cannot_authorize_a_one_short_marker_without_retained_history() {
    let mut receiver = entity(159);
    receiver.receive(at(0), &explicit_one_short_pdu()).unwrap();
    drain(&mut receiver);
    receiver
        .receive(at(1), &reset(159, false, false, 0))
        .unwrap();
    assert_eq!(send(&mut receiver, 2).unwrap().0, PublicationKind::ResetAck);
    let mut next = following_one_short_pdu();
    next[..2].copy_from_slice(&[0x80, 0x01]);
    receiver.receive(at(3), &next).unwrap();
    assert!(matches!(
        receiver.deliver_with(|_| Ok::<_, ()>(())),
        Err(PublishError::Rlc(AmError::Header))
    ));
    assert_eq!(receiver.counters().repeated_one_short_markers, 0);
}

#[test]
fn retained_pdu_evidence_survives_delivery_but_not_reuse_or_reset() {
    let mut receiver = entity(159);
    assert!(!receiver.status_history_enabled());
    receiver.enable_status_history();
    assert!(receiver.status_history_enabled());
    let mut pdu = explicit_one_short_pdu();
    pdu[158] = 0x45;
    receiver.receive(at(0), &pdu).unwrap();
    let before = receiver.diagnostics();
    let evidence = receiver.retained_pdu_evidence(0).unwrap();
    assert_eq!(receiver.diagnostics(), before);
    assert_eq!(evidence.position, 0);
    assert_eq!(evidence.boundary.last_indicator, Some(Indicator::End(154)));
    assert_eq!(evidence.padding_octet, Some(0x45));
    assert_eq!(evidence.first_data_octet, Some(0x51));
    assert_eq!(evidence.last_data_octet, Some(0x51));
    assert_eq!(drain(&mut receiver), vec![vec![0x51; 154]]);
    assert_eq!(receiver.retained_pdu_evidence(0), Some(evidence));
    let window = u64::from(config(159).receive_window.get());
    for position in 1..=window {
        super::wire::data_header(&mut pdu, position as u16, false, true);
        receiver.receive(at(position), &pdu).unwrap();
        assert_eq!(drain(&mut receiver), vec![vec![0x51; 154]]);
    }
    assert!(receiver.retained_pdu_evidence(0).is_none());
    assert_eq!(
        receiver.retained_pdu_evidence(window).unwrap().position,
        window
    );
    receiver
        .receive(at(window + 1), &reset(159, false, false, 0))
        .unwrap();
    assert!(receiver.retained_pdu_evidence(window).is_none());
}

#[test]
fn delivery_evidence_preserves_pending_boundary_and_does_not_advance_reassembly() {
    let mut receiver = entity(16);
    let mut bytes = [0; 16];
    bytes[..2].copy_from_slice(&0x8001_u16.to_be_bytes());
    assert_eq!(receiver.receive(at(0), &bytes), Ok(AmReceive::Stored));
    let evidence = receiver.delivery_evidence().unwrap();
    assert_eq!(evidence.header, 0x8001);
    assert_eq!(evidence.first_indicator, Some(Indicator::PreviousExact));
    assert_eq!(evidence.partial_bytes, 0);
    assert_eq!(evidence.partial_first, None);
    assert_eq!(evidence.parse_error, None);
    assert_eq!(receiver.delivery_evidence(), Some(evidence));
    assert!(matches!(
        receiver.deliver_with(|_| Ok::<_, ()>(())),
        Err(PublishError::Rlc(AmError::Header))
    ));
    assert_eq!(receiver.delivery_evidence(), Some(evidence));
}

#[test]
fn status_history_is_opt_in_bounded_and_records_only_accepted_publications() {
    let mut sender = entity(16);
    let mut receiver = entity(16);
    assert!(receiver.diagnostics().status_publications.is_none());
    receiver.enable_status_history();
    for index in 0..70_u64 {
        let tick = index * 1000;
        sender.enqueue(b"packet").unwrap();
        let (_, data) = send(&mut sender, tick).unwrap();
        receiver.receive(at(tick), &data).unwrap();
        drain(&mut receiver);
        let before = receiver.diagnostics().status_publications.unwrap();
        assert!(matches!(
            receiver.publish_next(at(tick), |_| Err::<(), _>(7)),
            Err(PublishError::Downstream(7))
        ));
        assert_eq!(receiver.diagnostics().status_publications, Some(before));
        let (kind, status) = send(&mut receiver, tick).unwrap();
        assert_eq!(kind, PublicationKind::Status);
        sender.receive(at(tick), &status).unwrap();
        receiver.enable_status_history();
    }
    let trace = receiver.diagnostics().status_publications.unwrap();
    assert_eq!(trace.published, 70);
    assert_eq!(trace.records.iter().flatten().count(), 64);
    let mut records: Vec<_> = trace.records.into_iter().flatten().collect();
    records.sort_by_key(|record| record.at.index);
    assert_eq!(records.first().unwrap().acknowledge_next, 7);
    assert_eq!(records.last().unwrap().acknowledge_next, 70);
    assert!(
        records
            .iter()
            .all(|record| record.complete && record.generation == 0)
    );
}

#[test]
fn peer_reset_preserves_pre_reset_state_across_ack_and_duplicate() {
    let mut sender = entity(16);
    let mut receiver = entity(16);
    sender.enqueue(b"packet").unwrap();
    let (_, data) = send(&mut sender, 0).unwrap();
    receiver.receive(at(0), &data).unwrap();
    let (_, status) = send(&mut receiver, 1).unwrap();
    sender.receive(at(1), &status).unwrap();
    let before = receiver.diagnostics();
    sender.initiate_reset(at(2)).unwrap();
    let (_, reset) = send(&mut sender, 2).unwrap();
    assert!(matches!(
        receiver.receive(at(2), &reset).unwrap(),
        AmReceive::ResetApplied { .. }
    ));
    let evidence = receiver.diagnostics().history.peer_reset.unwrap();
    assert_eq!(evidence.at, at(2));
    assert_eq!(evidence.rx_next, before.rx_next);
    assert_eq!(evidence.rx_high, before.rx_high);
    assert_eq!(evidence.tx_base, before.tx_base);
    assert_eq!(evidence.tx_next, before.tx_next);
    assert_eq!(evidence.counters, before.counters);
    assert_eq!(evidence.previous_published, before.history.last_published);
    assert_eq!(evidence.previous_received, before.history.last_received);
    assert_eq!(evidence.rx_deliver, 0);
    assert_eq!(send(&mut receiver, 3).unwrap().0, PublicationKind::ResetAck);
    assert_eq!(receiver.receive(at(4), &reset), Ok(AmReceive::Ignored));
    assert_eq!(receiver.diagnostics().history.peer_reset, Some(evidence));
    assert_ne!(receiver.diagnostics().rx_next, evidence.rx_next);
}

#[test]
fn padding_status_has_no_acknowledgment_or_sequence_side_effect() {
    let mut bytes = [0xff; 16];
    write_padding(&mut bytes).unwrap();
    assert_eq!(bytes, [0; 16]);
    let status = parse_status(&bytes).unwrap();
    assert_eq!(status.acknowledge, None);
    let mut sender = entity(16);
    sender.enqueue(b"pending").unwrap();
    let before = sender.window_positions();
    sender.receive(at(1), &bytes).unwrap();
    assert_eq!(sender.window_positions(), before);
    assert!(sender.pending_new_data());
    assert_eq!(write_padding(&mut []), Err(AmError::Length));
}

#[test]
fn transmission_readiness_uses_actual_window_poll_status_and_reset_state() {
    let mut sender = entity(16);
    assert!(!sender.publication_ready(at(0)).unwrap());
    sender.enqueue(b"packet").unwrap();
    assert!(sender.publication_ready(at(0)).unwrap());
    assert_eq!(sender.send_sequence(), 0);
    let (_, bytes) = send(&mut sender, 0).unwrap();
    let mut peer = entity(16);
    peer.receive(at(0), &bytes).unwrap();
    assert!(peer.publication_ready(at(0)).unwrap());
    assert!(matches!(
        send(&mut peer, 0).unwrap().0,
        PublicationKind::Status
    ));
    sender.initiate_reset(at(1)).unwrap();
    assert!(sender.publication_ready(at(1)).unwrap());
    assert!(matches!(
        send(&mut sender, 1).unwrap().0,
        PublicationKind::Reset
    ));
    assert!(!sender.publication_ready(at(2)).unwrap());
    assert!(sender.publication_ready(at(401)).unwrap());
}

fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}
fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
fn config(bytes: usize) -> AmConfig {
    AmConfig {
        transmit_pdu_bytes: size(bytes),
        receive_pdu_bytes: size(bytes),
        transmit_window: NonZeroU16::new(32).unwrap(),
        receive_window: NonZeroU16::new(32).unwrap(),
        maximum_data_attempts: NonZeroU16::new(10).unwrap(),
        maximum_reset_attempts: NonZeroU16::new(4).unwrap(),
        reset_timeout_chips: NonZeroU64::new(400).unwrap(),
        polling: Polling {
            every_pdus: Some(NonZeroU16::MIN),
            last_transmission: true,
            last_retransmission: true,
            timeout_chips: NonZeroU64::new(200),
            prohibit_chips: None,
        },
        status_prohibit_chips: NonZeroU64::new(100),
        missing_pdu_indicator: true,
        accept_special_he: false,
        initial_tx_hfn: 0,
        initial_rx_hfn: 0,
    }
}
fn limits() -> AmLimits {
    AmLimits {
        maximum_sdu_bytes: size(1024),
        queued_sdus: size(8),
        maximum_buffer_bytes: 1024 * 1024,
    }
}
fn entity(bytes: usize) -> AmEntity {
    AmEntity::new(config(bytes), limits(), at(0)).unwrap()
}

#[test]
fn prepared_receive_deciphers_before_li_parsing_and_failed_preparation_preserves_the_window() {
    let mut sender = entity(16);
    let mut receiver = entity(16);
    sender.enqueue(b"prepared bearer").unwrap();
    let (_, plain) = send(&mut sender, 0).unwrap();
    let mut encrypted = plain.clone();
    for byte in &mut encrypted[2..] {
        *byte ^= 0xa5
    }
    assert_eq!(
        receiver.receive_prepared(at(0), &encrypted, |_| Err::<&[u8], _>(7)),
        Err(ReceiveError::Preparation(7))
    );
    assert_eq!(receiver.receive_sequence(), 0);
    assert_eq!(receiver.counters().received, 0);
    let mut scratch = encrypted.clone();
    assert_eq!(
        receiver.receive_prepared(at(0), &encrypted, |request| {
            assert_eq!(
                request,
                ReceivePreparation::Data {
                    sequence: 0,
                    position: 0
                }
            );
            for byte in &mut scratch[2..] {
                *byte ^= 0xa5
            }
            Ok::<_, ()>(scratch.as_slice())
        }),
        Ok(AmReceive::Stored)
    );
    assert_eq!(receiver.receive_sequence(), 1);
    let mut delivered = drain(&mut receiver);
    for now in 1..=4 {
        let Some((_, bytes)) = send(&mut sender, now) else {
            break;
        };
        receiver.receive(at(now), &bytes).unwrap();
        delivered.extend(drain(&mut receiver));
        if !delivered.is_empty() {
            break;
        }
    }
    assert_eq!(delivered, vec![b"prepared bearer".to_vec()]);
}

#[test]
fn outside_window_poll_does_not_require_deciphering_or_li_parsing() {
    let mut receiver = entity(16);
    let mut bytes = vec![0xff; 16];
    wire::data_header(&mut bytes, 32, true, true);
    assert_eq!(
        receiver.receive_prepared(at(0), &bytes, |_| -> Result<&[u8], ()> {
            panic!("outside-window PDU reached deciphering")
        }),
        Ok(AmReceive::OutsideWindow)
    );
    assert_eq!(receiver.counters().outside_window, 1);
    assert_eq!(receiver.receive_sequence(), 0);
    assert!(matches!(
        send(&mut receiver, 0),
        Some((PublicationKind::Status, _))
    ));
}

#[test]
fn prepared_receive_cannot_change_the_sequence_header_or_skip_buffered_duplicates() {
    let mut sender = entity(16);
    let mut receiver = entity(16);
    sender.enqueue(b"duplicate").unwrap();
    let (_, mut bytes) = send(&mut sender, 0).unwrap();
    wire::data_header(&mut bytes, 1, true, true);
    let mut changed = bytes.clone();
    changed[1] ^= 8;
    assert_eq!(
        receiver.receive_prepared(at(0), &bytes, |_| Ok::<_, ()>(changed.as_slice())),
        Err(ReceiveError::Rlc(AmError::Header))
    );
    let mut prepared = 0;
    for expected in [AmReceive::Stored, AmReceive::Duplicate] {
        assert_eq!(
            receiver.receive_prepared(at(0), &bytes, |request| {
                assert_eq!(
                    request,
                    ReceivePreparation::Data {
                        sequence: 1,
                        position: 1
                    }
                );
                prepared += 1;
                Ok::<_, ()>(bytes.as_slice())
            }),
            Ok(expected)
        );
    }
    assert_eq!(prepared, 2);
    assert_eq!(receiver.counters().received, 1);
    assert_eq!(receiver.counters().duplicate, 1);
    assert_eq!(receiver.receive_sequence(), 0);
}

#[test]
fn mapped_counts_do_not_rewind_transport_and_reset_uses_the_last_new_transmission_not_an_old_retransmission()
 {
    let mut sender_config = config(16);
    sender_config.initial_tx_hfn = 3;
    let mut sender = AmEntity::new(sender_config, limits(), at(0)).unwrap();
    let mut peer_config = config(16);
    peer_config.initial_tx_hfn = 7;
    peer_config.initial_rx_hfn = 3;
    let mut peer = AmEntity::new(peer_config, limits(), at(0)).unwrap();
    let origin = 3u64 << SN_BITS;
    let map = |position| {
        Ok::<_, ()>(if position == origin {
            3 << SN_BITS
        } else {
            (position - origin) as u32
        })
    };
    for (now, payload) in [(0, b"old".as_slice()), (1, b"new".as_slice())] {
        sender.enqueue(payload).unwrap();
        let (kind, position) = sender
            .publish_next_mapped(at(now), map, |publication| {
                Ok::<_, ()>(publication.position.unwrap())
            })
            .unwrap()
            .unwrap();
        assert_eq!(position, origin + now);
        assert_eq!(
            kind,
            PublicationKind::Data {
                sequence: now as u16,
                count: if now == 0 { 3 << SN_BITS } else { 1 },
                retransmission: false
            }
        );
    }
    sender.receive(at(2), &status(16, 2, &[0])).unwrap();
    let (kind, ()) = sender
        .publish_next_mapped(at(3), map, |publication| {
            assert_eq!(publication.position, Some(origin));
            Ok::<_, ()>(())
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        kind,
        PublicationKind::Data {
            sequence: 0,
            count: 3 << SN_BITS,
            retransmission: true
        }
    );
    sender.initiate_reset(at(4)).unwrap();
    let (PublicationKind::Reset, reset) = send(&mut sender, 4).unwrap() else {
        panic!("not RESET")
    };
    assert_eq!(parse_reset(&reset).unwrap().hfn, 0);
    let mut planned = None;
    assert_eq!(
        peer.receive_prepared(at(4), &reset, |request| {
            let ReceivePreparation::Reset { transition, .. } = request else {
                panic!("not a reset transition")
            };
            planned = transition;
            Ok::<_, ()>(reset.as_slice())
        }),
        Ok(AmReceive::ResetApplied {
            tx_hfn: 8,
            rx_hfn: 1
        })
    );
    let planned = planned.unwrap();
    assert_eq!(peer.window_positions().next_receive, planned.rx_position);
    assert!(planned.rx_position > origin);
    assert_eq!(peer.receive_count(0), Ok(1 << SN_BITS));
    let (PublicationKind::ResetAck, ack) = send(&mut peer, 4).unwrap() else {
        panic!("not RESET ACK")
    };
    assert_eq!(parse_reset(&ack).unwrap().hfn, 7);
    assert_eq!(
        sender.receive(at(5), &ack),
        Ok(AmReceive::ResetAcknowledged {
            tx_hfn: 1,
            rx_hfn: 8
        })
    );
    assert!(sender.window_positions().next_transmit > origin + 1);
    sender.enqueue(b"after reset").unwrap();
    let (kind, position) = sender
        .publish_next(at(5), |publication| {
            Ok::<_, ()>(publication.position.unwrap())
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        kind,
        PublicationKind::Data {
            sequence: 0,
            count: 1 << SN_BITS,
            retransmission: false
        }
    );
    assert!(position > origin + 1);
    let before = peer.window_positions();
    assert_eq!(
        peer.receive_prepared(at(6), &reset, |request| {
            assert!(matches!(
                request,
                ReceivePreparation::Reset {
                    transition: None,
                    ..
                }
            ));
            Ok::<_, ()>(reset.as_slice())
        }),
        Ok(AmReceive::Ignored)
    );
    assert_eq!(peer.window_positions(), before);
}

#[test]
fn count_mapping_fails_before_publication_and_live_windows_cannot_be_reseeded() {
    let mut sender = entity(16);
    sender.reseed_idle_counters(9, 11).unwrap();
    sender.enqueue(b"count mapping").unwrap();
    assert_eq!(
        sender.publish_next_mapped(
            at(0),
            |_| Err::<u32, _>(7),
            |_| -> Result<(), i32> { panic!("failed mapper published a PDU") }
        ),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(sender.send_sequence(), 0);
    assert_eq!(
        sender.publish_next_mapped(
            at(0),
            |_| Ok::<_, ()>(1),
            |_| -> Result<(), ()> { panic!("mismatched COUNT published a PDU") }
        ),
        Err(PublishError::Rlc(AmError::Header))
    );
    let (kind, _) = send(&mut sender, 0).unwrap();
    assert_eq!(
        kind,
        PublicationKind::Data {
            sequence: 0,
            count: 9 << SN_BITS,
            retransmission: false
        }
    );
    assert_eq!(sender.reseed_idle_counters(0, 0), Err(AmError::State));
    sender.receive(at(1), &status(16, 1, &[])).unwrap();
    assert_eq!(sender.outstanding_pdus(), 0);
    assert_eq!(sender.reseed_idle_counters(0, 0), Err(AmError::State));
}
fn send(entity: &mut AmEntity, now: u64) -> Option<(PublicationKind, Vec<u8>)> {
    entity.check_retransmission_count();
    let result = entity
        .publish_next(at(now), |pdu| Ok::<_, ()>(pdu.bytes.to_vec()))
        .unwrap();
    entity.check_retransmission_count();
    result
}

#[test]
fn retransmission_count_survives_duplicate_status_failure_ring_reuse_and_reset() {
    let mut sender = entity(16);
    for round in 0..80 {
        let now = round * 10;
        let sequence = sender.send_sequence();
        sender.enqueue(b"payload").unwrap();
        assert!(matches!(
            send(&mut sender, now).unwrap().0,
            PublicationKind::Data {
                retransmission: false,
                ..
            }
        ));
        let report = status(16, sender.send_sequence(), &[sequence]);
        for _ in 0..2 {
            sender.receive(at(now + 1), &report).unwrap();
            sender.check_retransmission_count();
        }
        assert!(matches!(
            sender.publish_next(at(now + 2), |_| Err::<(), _>(())),
            Err(PublishError::Downstream(()))
        ));
        sender.check_retransmission_count();
        assert!(matches!(
            send(&mut sender, now + 3).unwrap().0,
            PublicationKind::Data {
                retransmission: true,
                ..
            }
        ));
        sender.receive(at(now + 4), &report).unwrap();
        sender.check_retransmission_count();
        let acknowledge = status(16, sender.send_sequence(), &[]);
        sender.receive(at(now + 5), &acknowledge).unwrap();
        sender.check_retransmission_count();
        assert_eq!(sender.outstanding_pdus(), 0);
    }
    sender.enqueue(b"reset").unwrap();
    let sequence = sender.send_sequence();
    send(&mut sender, 800).unwrap();
    let report = status(16, sender.send_sequence(), &[sequence]);
    sender.receive(at(801), &report).unwrap();
    sender.check_retransmission_count();
    let mut reset = [0; 16];
    wire::write_reset(
        &mut reset,
        Reset {
            acknowledge: false,
            sequence: false,
            hfn: 0,
        },
    )
    .unwrap();
    assert!(matches!(
        sender.receive(at(802), &reset).unwrap(),
        AmReceive::ResetApplied { .. }
    ));
    sender.check_retransmission_count();
    assert_eq!(sender.outstanding_pdus(), 0);
    assert!(matches!(
        send(&mut sender, 803).unwrap().0,
        PublicationKind::ResetAck
    ));
}
fn drain(entity: &mut AmEntity) -> Vec<Vec<u8>> {
    let mut result = Vec::new();
    for _ in 0..64 {
        if entity
            .deliver_with(|delivery| {
                result.extend(delivery.sdus().map(<[u8]>::to_vec));
                Ok::<_, ()>(())
            })
            .unwrap()
            .is_none()
        {
            return result;
        }
    }
    panic!("delivery work exceeded bound")
}
fn status(size: usize, ack: u16, negative: &[u16]) -> Vec<u8> {
    let mut bytes = vec![0; size];
    let mut writer = super::status::Writer::new(&mut bytes);
    for &sequence in negative {
        writer.missing_run(sequence, 1).unwrap();
    }
    writer.ack(ack).unwrap();
    bytes
}
fn reset(size: usize, sequence: bool, acknowledge: bool, hfn: u32) -> Vec<u8> {
    let mut bytes = vec![0; size];
    wire::write_reset(
        &mut bytes,
        Reset {
            acknowledge,
            sequence,
            hfn,
        },
    )
    .unwrap();
    bytes
}

#[test]
fn literal_amd_reset_and_ack_vectors_match_the_spec_bit_fields() {
    let bytes = [0xd5, 0xe5, 0x07, 0xfe, b'a', b'b', b'c', 0x55];
    let pdu = parse_amd(&bytes, LiWidth::Seven, false).unwrap();
    assert_eq!((pdu.sequence, pdu.poll, pdu.complete), (0xabc, true, false));
    assert_eq!(pdu.indicators(), &[Indicator::End(3), Indicator::Padding]);
    assert_eq!(pdu.data, b"abc");
    assert_eq!(
        parse_reset(&[0x18, 0xab, 0xcd, 0xe0]).unwrap(),
        Reset {
            acknowledge: false,
            sequence: true,
            hfn: 0xabcde
        }
    );
    assert_eq!(
        parse_reset(&[0x20, 0x12, 0x34, 0x50]).unwrap(),
        Reset {
            acknowledge: true,
            sequence: false,
            hfn: 0x12345
        }
    );
    assert_eq!(
        parse_status(&[0x02, 0xab, 0xc0]).unwrap().acknowledge,
        Some(0xabc)
    );
}

#[test]
fn fifteen_bit_li_vector_uses_big_endian_cumulative_octet_offsets() {
    let mut input = vec![0xa5; 306];
    input[..6].copy_from_slice(&[0x80, 0x01, 0x02, 0x59, 0xff, 0xfe]);
    let pdu = parse_amd(&input, LiWidth::Fifteen, false).unwrap();
    assert_eq!(pdu.indicators(), &[Indicator::End(300), Indicator::Padding]);
    assert_eq!(pdu.data, &[0xa5; 300]);
}

#[test]
fn status_coalesces_burst_losses_without_omitting_holes() {
    let mut configuration = config(40);
    configuration.transmit_window = NonZeroU16::new(256).unwrap();
    configuration.receive_window = NonZeroU16::new(256).unwrap();
    let mut cap = limits();
    cap.maximum_buffer_bytes = 2 * 1024 * 1024;
    let mut receiver = AmEntity::new(configuration, cap, at(0)).unwrap();
    let mut packet = vec![0; 40];
    wire::data_header(&mut packet, 200, true, true);
    packet[2] = 0xfe;
    receiver.receive(at(1), &packet).unwrap();
    let (kind, bytes) = send(&mut receiver, 2).unwrap();
    assert_eq!(kind, PublicationKind::Status);
    let status = parse_status(&bytes).unwrap();
    assert_eq!(status.acknowledge, Some(0));
    assert_eq!(
        status.negative.iter().collect::<Vec<_>>(),
        (0..200).collect::<Vec<_>>()
    );
    assert!(send(&mut receiver, 4).is_none());
}

#[test]
fn packed_status_lists_fragment_without_skipping_isolated_losses() {
    let mut configuration = config(40);
    configuration.receive_window = NonZeroU16::new(256).unwrap();
    let mut receiver = AmEntity::new(configuration, limits(), at(0)).unwrap();
    let mut packet = [0; 40];
    for sequence in (1..202).step_by(2) {
        wire::data_header(&mut packet, sequence, true, true);
        packet[2] = 0xfe;
        receiver.receive(at(1), &packet).unwrap();
    }
    assert!(matches!(
        receiver.publish_next(at(2), |_| Err::<(), _>(7)),
        Err(PublishError::Downstream(7))
    ));
    let mut missing = Vec::new();
    let mut fragments = 0;
    for now in 2..20 {
        let Some((kind, bytes)) = send(&mut receiver, now) else {
            break;
        };
        assert_eq!(kind, PublicationKind::Status);
        let status = parse_status(&bytes).unwrap();
        assert_eq!(status.acknowledge, Some(0));
        missing.extend(status.negative.iter());
        fragments += 1;
    }
    assert_eq!(fragments, 6);
    assert_eq!(missing, (0..202).step_by(2).collect::<Vec<_>>());
    assert!(send(&mut receiver, 20).is_none());
}

#[test]
fn status_run_wire_vector_wraps_sequence_and_reserves_ack_space() {
    let mut bytes = [0; 6];
    let mut writer = super::status::Writer::new(&mut bytes);
    writer.missing_run(4094, 4).unwrap();
    assert!(!writer.can_missing());
    writer.ack(2).unwrap();
    assert_eq!(bytes, [0x03, 0x1f, 0xfe, 0x32, 0x00, 0x20]);
    let status = parse_status(&bytes).unwrap();
    assert_eq!(status.acknowledge, Some(2));
    assert_eq!(
        status.negative.iter().collect::<Vec<_>>(),
        [0, 1, 4094, 4095]
    );
}

#[test]
fn peer_window_shrink_blocks_new_data_but_keeps_the_last_pdu_retransmittable() {
    let mut sender = entity(12);
    sender.enqueue(&[0x54; 45]).unwrap();
    for now in 1..=3 {
        send(&mut sender, now).unwrap();
    }
    let mut shrink = vec![0; 12];
    shrink[..5].copy_from_slice(&[0x01, 0x00, 0x12, 0, 0]);
    sender.receive(at(4), &shrink).unwrap();
    assert!(send(&mut sender, 5).is_none());
    sender.receive(at(6), &status(12, 0, &[2])).unwrap();
    assert_eq!(
        send(&mut sender, 7).unwrap().0,
        PublicationKind::Data {
            sequence: 2,
            count: 2,
            retransmission: true
        }
    );
    sender.receive(at(8), &status(12, 3, &[])).unwrap();
    assert_eq!(
        send(&mut sender, 9).unwrap().0,
        PublicationKind::Data {
            sequence: 3,
            count: 3,
            retransmission: false
        }
    );
}

#[test]
fn literal_list_bitmap_and_relative_list_statuses_keep_exact_missing_sequences() {
    let list = parse_status(&[0x03, 0x11, 0x23, 0x22, 0x13, 0x00]).unwrap();
    assert_eq!(
        list.negative.iter().collect::<Vec<_>>(),
        [0x123, 0x124, 0x125]
    );
    assert_eq!(list.acknowledge, Some(0x130));
    let bitmap = parse_status(&[0x04, 0x01, 0x00, 0xa5, 0x21, 0x08]).unwrap();
    assert_eq!(
        bitmap.positive.iter().collect::<Vec<_>>(),
        [0x100, 0x102, 0x105, 0x107]
    );
    assert_eq!(
        bitmap.negative.iter().collect::<Vec<_>>(),
        [0x101, 0x103, 0x104, 0x106]
    );
    let relative = parse_status(&[0x05, 0x30, 0x10, 0x51, 0x72, 0x02, 0x00]).unwrap();
    assert_eq!(
        relative.negative.iter().collect::<Vec<_>>(),
        [0x10, 0x12, 0x13, 0x14, 0x15]
    );
    assert_eq!(relative.acknowledge, Some(0x20));
    let multiword = parse_status(&[0x05, 0x20, 0x10, 0x23, 0x20, 0x20]).unwrap();
    assert_eq!(multiword.negative.iter().collect::<Vec<_>>(), [0x10, 0x19]);
}

#[test]
fn malformed_reserved_truncated_and_inconsistent_inputs_are_rejected() {
    for bytes in [
        &[0x80, 0x03, 0][..],
        &[0x80, 0x01, 0xf8, 0][..],
        &[0x80, 0x01, 0xfa, 0][..],
        &[0x80, 0x01, 0x11][..],
        &[0x80, 0x01, 0x03, 0, 0][..],
    ] {
        assert!(parse_amd(bytes, LiWidth::Seven, false).is_err());
    }
    for bytes in [
        &[0x03, 0][..],
        &[0x09, 0][..],
        &[0x05, 0x10, 0x00, 0x20][..],
        &[0x05, 0x10, 0x00, 0x10][..],
    ] {
        assert!(parse_status(bytes).is_err());
    }
    assert!(parse_reset(&[0x19, 0, 0, 0]).is_err());
    assert!(parse_reset(&[0x30, 0, 0, 0]).is_err());
    assert!(parse_reset(&[0x10, 0, 0]).is_err());
    let inconsistent = [0x03, 0x10, 0x00, 0x04, 0x00, 0x00, 0xff, 0x00];
    assert_eq!(
        parse_status(&inconsistent).err(),
        Some(AmError::InconsistentStatus)
    );
    assert_eq!(parse_status(&[0x06, 0]).err(), Some(AmError::Header));
}

#[test]
fn piggybacked_status_and_special_he_are_distinct_valid_constructions() {
    let bytes = [
        0x80, 0x05, 0x05, 0xfc, b'h', b'i', 0x02, 0, 0x10, 0xff, 0xff, 0xff,
    ];
    let pdu = parse_amd(&bytes, LiWidth::Seven, false).unwrap();
    assert_eq!(pdu.data, b"hi");
    assert_eq!(
        parse_status(pdu.piggybacked_status.unwrap())
            .unwrap()
            .acknowledge,
        Some(1)
    );
    assert!(parse_amd(&[0x80, 0x02, 1, 2, 3, 4], LiWidth::Seven, false).is_err());
    assert!(
        parse_amd(&[0x80, 0x02, 1, 2, 3, 4], LiWidth::Seven, true)
            .unwrap()
            .complete
    );
}

#[test]
fn failed_publication_preserves_sequence_payload_and_count_identity() {
    let mut sender = entity(12);
    sender.enqueue(b"hello").unwrap();
    let mut attempted = Vec::new();
    assert_eq!(
        sender.publish_next(at(1), |pdu| {
            attempted = pdu.bytes.to_vec();
            Err::<(), _>("full")
        }),
        Err(PublishError::Downstream("full"))
    );
    assert_eq!(sender.send_sequence(), 0);
    assert_eq!(sender.outstanding_pdus(), 0);
    let (kind, actual) = send(&mut sender, 2).unwrap();
    assert_eq!(actual, attempted);
    assert_eq!(
        kind,
        PublicationKind::Data {
            sequence: 0,
            count: 0,
            retransmission: false
        }
    );
    assert_eq!(sender.counters().publication_failures, 1);
}

#[test]
fn out_of_order_reception_requests_the_hole_and_retransmits_original_bytes_in_order() {
    let mut sender = entity(12);
    let mut receiver = entity(12);
    let input = (0..18).collect::<Vec<_>>();
    sender.enqueue(&input).unwrap();
    let (_, first) = send(&mut sender, 1).unwrap();
    let (_, second) = send(&mut sender, 2).unwrap();
    assert_eq!(receiver.receive(at(10), &second), Ok(AmReceive::Stored));
    assert_eq!(receiver.receive(at(11), &second), Ok(AmReceive::Duplicate));
    assert!(drain(&mut receiver).is_empty());
    let (kind, report) = send(&mut receiver, 12).unwrap();
    assert_eq!(kind, PublicationKind::Status);
    assert_eq!(
        parse_status(&report)
            .unwrap()
            .negative
            .iter()
            .collect::<Vec<_>>(),
        [0]
    );
    assert_eq!(
        sender.receive(at(13), &report),
        Ok(AmReceive::StatusApplied)
    );
    let (kind, retransmit) = send(&mut sender, 14).unwrap();
    assert_eq!(
        kind,
        PublicationKind::Data {
            sequence: 0,
            count: 0,
            retransmission: true
        }
    );
    assert_eq!(retransmit, first);
    receiver.receive(at(15), &retransmit).unwrap();
    assert_eq!(receiver.receive_sequence(), 2);
    assert_eq!(drain(&mut receiver), [input]);
    let (_, report) = send(&mut receiver, 120).unwrap();
    sender.receive(at(121), &report).unwrap();
    assert_eq!(sender.outstanding_pdus(), 0);
}

#[test]
fn upper_queue_rejection_and_receive_buffer_pressure_do_not_lose_sdus() {
    let mut configuration = config(12);
    configuration.receive_window = NonZeroU16::new(2).unwrap();
    let mut receiver = AmEntity::new(configuration, limits(), at(0)).unwrap();
    let mut sender = entity(12);
    let mut packets = Vec::new();
    for value in 0..3 {
        sender.enqueue(&[value; 8]).unwrap();
        packets.push(send(&mut sender, value as u64 + 1).unwrap().1);
    }
    receiver.receive(at(4), &packets[0]).unwrap();
    receiver.receive(at(5), &packets[1]).unwrap();
    assert_eq!(receiver.receive(at(6), &packets[2]), Err(AmError::Capacity));
    let mut attempted = Vec::new();
    assert_eq!(
        receiver.deliver_with(|delivery| {
            attempted.extend(delivery.sdus().map(<[u8]>::to_vec));
            Err::<(), _>("full")
        }),
        Err(PublishError::Downstream("full"))
    );
    assert_eq!(receiver.counters().delivered_sdus, 0);
    let delivered = drain(&mut receiver);
    assert_eq!(delivered.first(), attempted.first());
    receiver.receive(at(7), &packets[2]).unwrap();
    let mut all = delivered;
    all.extend(drain(&mut receiver));
    assert_eq!(all, [vec![0; 8], vec![1; 8], vec![2; 8]]);
}

#[test]
fn fixed_size_and_budget_bounds_reject_before_allocating_or_mutating() {
    let configuration = config(12);
    let mut cap = limits();
    cap.maximum_buffer_bytes = configuration.buffer_bytes(cap).unwrap() - 1;
    assert_eq!(
        AmEntity::new(configuration, cap, at(0)).err(),
        Some(AmError::Capacity)
    );
    for bytes in [1, 4, 5, MAX_PDU_BYTES + 1] {
        assert_eq!(
            AmEntity::new(config(bytes), limits(), at(0)).err(),
            Some(AmError::Capacity)
        );
    }
    let mut receiver = entity(12);
    assert_eq!(receiver.receive(at(1), &[0x80; 11]), Err(AmError::Length));
    assert_eq!(receiver.receive_sequence(), 0);
    let mut bad = config(12);
    bad.transmit_window = NonZeroU16::new(2048).unwrap();
    assert!(AmEntity::new(bad, limits(), at(0)).is_err());
    bad = config(12);
    bad.initial_tx_hfn = MAX_HFN + 1;
    assert!(AmEntity::new(bad, limits(), at(0)).is_err());
}

#[test]
fn erroneous_status_starts_reset_without_advancing_the_send_window() {
    let mut sender = entity(12);
    sender.enqueue(b"hello").unwrap();
    send(&mut sender, 1).unwrap();
    assert_eq!(
        sender.receive(at(2), &status(12, 2, &[])),
        Ok(AmReceive::ResetStarted)
    );
    assert_eq!(sender.acknowledge_sequence(), 0);
    assert_eq!(sender.outstanding_pdus(), 1);
    assert_eq!(sender.state(), AmState::ResetPending);
    let diagnostic = sender.diagnostics();
    assert_eq!(
        diagnostic.history.reset_started,
        Some(ResetStart {
            at: at(2),
            trigger: ResetTrigger::InvalidStatus {
                acknowledge: Some(2),
                negative: None
            },
            tx_base: 0,
            tx_next: 1,
            rx_next: 0,
            rx_high: 0,
        })
    );
    assert_eq!(
        diagnostic.history.last_received.unwrap().prefix[..12],
        status(12, 2, &[])
    );
    assert_eq!(send(&mut sender, 3).unwrap().0, PublicationKind::Reset);
    assert_eq!(
        sender.receive(at(4), &status(12, 1, &[])),
        Ok(AmReceive::Ignored)
    );
}

#[test]
fn cumulative_ack_stops_at_first_error_and_retransmission_precedes_new_data() {
    let mut sender = entity(12);
    sender.enqueue(&[0x7e; 35]).unwrap();
    for now in 1..=3 {
        send(&mut sender, now).unwrap();
    }
    sender.receive(at(4), &status(12, 3, &[1])).unwrap();
    assert_eq!(sender.acknowledge_sequence(), 1);
    assert_eq!(
        send(&mut sender, 5).unwrap().0,
        PublicationKind::Data {
            sequence: 1,
            count: 1,
            retransmission: true
        }
    );
    assert_eq!(
        send(&mut sender, 6).unwrap().0,
        PublicationKind::Data {
            sequence: 3,
            count: 3,
            retransmission: false
        }
    );
}

#[test]
fn poll_timeout_reuses_the_original_count_and_starts_only_after_queue_acceptance() {
    let mut sender = entity(12);
    sender.enqueue(b"hello").unwrap();
    let (_, original) = send(&mut sender, 10).unwrap();
    assert!(send(&mut sender, 209).is_none());
    assert_eq!(
        sender.publish_next(at(210), |_| Err::<(), _>("full")),
        Err(PublishError::Downstream("full"))
    );
    let (kind, packet) = send(&mut sender, 211).unwrap();
    assert_eq!(packet, original);
    assert_eq!(
        kind,
        PublicationKind::Data {
            sequence: 0,
            count: 0,
            retransmission: true
        }
    );
    assert!(send(&mut sender, 410).is_none());
    sender.receive(at(411), &status(12, 1, &[])).unwrap();
    assert!(send(&mut sender, 999).is_none());
}

#[test]
fn poll_prohibit_delays_one_trigger_and_status_prohibit_starts_after_final_fragment() {
    let mut configuration = config(6);
    configuration.polling.prohibit_chips = NonZeroU64::new(20);
    let mut sender = AmEntity::new(configuration, limits(), at(0)).unwrap();
    sender.enqueue(&[0x34; 13]).unwrap();
    let (_, first) = send(&mut sender, 1).unwrap();
    let (_, second) = send(&mut sender, 2).unwrap();
    assert!(parse_amd(&first, LiWidth::Seven, false).unwrap().poll);
    assert!(!parse_amd(&second, LiWidth::Seven, false).unwrap().poll);
    let mut receiver = entity(6);
    receiver.receive(at(2), &second).unwrap();
    for now in 3..=4 {
        let (_, packet) = send(&mut sender, now).unwrap();
        if now == 4 {
            receiver.receive(at(now), &packet).unwrap();
        }
    }
    let (_, delayed_poll) = send(&mut sender, 21).unwrap();
    assert!(
        parse_amd(&delayed_poll, LiWidth::Seven, false)
            .unwrap()
            .poll
    );
    let (_, first_status) = send(&mut receiver, 10).unwrap();
    assert_eq!(
        parse_status(&first_status)
            .unwrap()
            .negative
            .iter()
            .collect::<Vec<_>>(),
        [0]
    );
    let (_, second_status) = send(&mut receiver, 11).unwrap();
    assert_eq!(
        parse_status(&second_status)
            .unwrap()
            .negative
            .iter()
            .collect::<Vec<_>>(),
        [2]
    );
    assert!(send(&mut receiver, 12).is_none());
    receiver.receive(at(13), &first).unwrap();
    assert!(send(&mut receiver, 110).is_none());
    assert_eq!(send(&mut receiver, 111).unwrap().0, PublicationKind::Status);
}

#[test]
fn reset_handshake_synchronizes_both_hfns_and_preserves_unstarted_sdus() {
    let mut sender_config = config(12);
    sender_config.initial_tx_hfn = 7;
    sender_config.initial_rx_hfn = 11;
    let mut receiver_config = config(12);
    receiver_config.initial_tx_hfn = 11;
    receiver_config.initial_rx_hfn = 7;
    let mut sender = AmEntity::new(sender_config, limits(), at(0)).unwrap();
    let mut receiver = AmEntity::new(receiver_config, limits(), at(0)).unwrap();
    sender.enqueue(&[0xaa; 100]).unwrap();
    sender.enqueue(b"keep").unwrap();
    send(&mut sender, 1).unwrap();
    sender.initiate_reset(at(2)).unwrap();
    let (_, request) = send(&mut sender, 3).unwrap();
    assert_eq!(
        receiver.receive(at(4), &request),
        Ok(AmReceive::ResetApplied {
            tx_hfn: 12,
            rx_hfn: 8
        })
    );
    let (_, response) = send(&mut receiver, 5).unwrap();
    assert_eq!(
        sender.receive(at(6), &response),
        Ok(AmReceive::ResetAcknowledged {
            tx_hfn: 8,
            rx_hfn: 12
        })
    );
    let (kind, packet) = send(&mut sender, 7).unwrap();
    assert_eq!(
        kind,
        PublicationKind::Data {
            sequence: 0,
            count: 8 << SN_BITS,
            retransmission: false
        }
    );
    receiver.receive(at(8), &packet).unwrap();
    assert_eq!(drain(&mut receiver), [b"keep".to_vec()]);
}

#[test]
fn duplicate_peer_reset_replays_the_cached_ack_without_incrementing_hfns_again() {
    let mut receiver = entity(12);
    let request = reset(12, false, false, 42);
    assert_eq!(
        receiver.receive(at(1), &request),
        Ok(AmReceive::ResetApplied {
            tx_hfn: 1,
            rx_hfn: 43
        })
    );
    let (_, first) = send(&mut receiver, 2).unwrap();
    assert_eq!(receiver.receive(at(3), &request), Ok(AmReceive::Ignored));
    let (_, repeated) = send(&mut receiver, 4).unwrap();
    assert_eq!(first, repeated);
    assert_eq!(receiver.counters().resets, 1);
    assert_eq!(receiver.receive_count(0), Ok(43 << SN_BITS));
}

#[test]
fn simultaneous_resets_do_not_cancel_the_local_reset_timer() {
    let mut one = entity(12);
    let mut two = entity(12);
    one.initiate_reset(at(1)).unwrap();
    two.initiate_reset(at(1)).unwrap();
    let (_, left) = send(&mut one, 2).unwrap();
    let (_, right) = send(&mut two, 2).unwrap();
    one.receive(at(3), &right).unwrap();
    two.receive(at(3), &left).unwrap();
    assert_eq!(one.state(), AmState::ResetPending);
    assert_eq!(two.state(), AmState::ResetPending);
    let (_, left_ack) = send(&mut one, 4).unwrap();
    let (_, right_ack) = send(&mut two, 4).unwrap();
    assert_eq!(
        one.receive(at(5), &right_ack),
        Ok(AmReceive::ResetAcknowledged {
            tx_hfn: 1,
            rx_hfn: 1
        })
    );
    assert_eq!(
        two.receive(at(5), &left_ack),
        Ok(AmReceive::ResetAcknowledged {
            tx_hfn: 1,
            rx_hfn: 1
        })
    );
    assert_eq!(one.state(), AmState::Ready);
    assert!(send(&mut one, 500).is_none());
}

#[test]
fn maxdat_and_maxrst_limits_are_one_less_actual_transmission_than_their_values() {
    let mut configuration = config(12);
    configuration.maximum_data_attempts = NonZeroU16::new(2).unwrap();
    configuration.maximum_reset_attempts = NonZeroU16::new(3).unwrap();
    let mut sender = AmEntity::new(configuration, limits(), at(0)).unwrap();
    sender.enqueue(b"hello").unwrap();
    send(&mut sender, 1).unwrap();
    assert!(send(&mut sender, 201).is_none());
    assert_eq!(sender.state(), AmState::ResetPending);
    let (_, first) = send(&mut sender, 202).unwrap();
    assert!(send(&mut sender, 601).is_none());
    let (_, second) = send(&mut sender, 602).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        sender.publish_next(at(1002), |_| Ok::<_, ()>(())),
        Err(PublishError::Rlc(AmError::ResetLimit))
    );
    assert_eq!(sender.state(), AmState::Failed);
    assert_eq!(sender.counters().transmitted, 1);
    let diagnostic = sender.diagnostics();
    assert_eq!(
        diagnostic.history.reset_started.unwrap().trigger,
        ResetTrigger::DataAttempts {
            sequence: 0,
            attempts: 1
        }
    );
    assert_eq!(diagnostic.history.resets_published, 2);
    assert_eq!(diagnostic.history.last_published.unwrap().at, at(602));
    assert_eq!(
        diagnostic.history.last_published.unwrap().prefix[..12],
        second
    );
}

#[test]
fn standard_limit_one_values_fail_without_emitting_data_or_reset_pdus() {
    let mut configuration = config(12);
    configuration.maximum_data_attempts = NonZeroU16::MIN;
    configuration.maximum_reset_attempts = NonZeroU16::MIN;
    let mut sender = AmEntity::new(configuration, limits(), at(0)).unwrap();
    sender.enqueue(b"unsent").unwrap();
    assert_eq!(
        sender.publish_next(at(1), |_| -> Result<(), ()> {
            panic!("MaxDAT 1 permits no data transmission")
        }),
        Ok(None)
    );
    assert_eq!(sender.state(), AmState::ResetPending);
    assert_eq!(
        sender.publish_next(at(2), |_| -> Result<(), ()> {
            panic!("MaxRST 1 permits no reset transmission")
        }),
        Err(PublishError::Rlc(AmError::ResetLimit))
    );
    assert_eq!(sender.state(), AmState::Failed);
    assert_eq!(sender.counters().transmitted, 0);
}

#[test]
fn failed_reset_publication_does_not_start_timer_or_consume_attempts() {
    let mut sender = entity(12);
    sender.initiate_reset(at(1)).unwrap();
    assert_eq!(
        sender.publish_next(at(2), |_| Err::<(), _>("full")),
        Err(PublishError::Downstream("full"))
    );
    assert_eq!(sender.diagnostics().history.resets_published, 0);
    assert!(sender.diagnostics().history.last_published.is_none());
    let (_, first) = send(&mut sender, 3).unwrap();
    assert!(!parse_reset(&first).unwrap().sequence);
    assert!(send(&mut sender, 402).is_none());
    assert_eq!(
        sender.receive(at(403), &reset(12, true, true, 0)),
        Ok(AmReceive::Ignored)
    );
    let (_, repeated) = send(&mut sender, 404).unwrap();
    assert_eq!(first, repeated);
}

#[test]
fn both_li_widths_cover_concatenation_exact_and_one_short_boundaries() {
    for bytes in [12, 128, 160] {
        for length in [bytes - 3, bytes - 2, bytes + 2, 3 * bytes + 3] {
            let mut sender = entity(bytes);
            let mut receiver = entity(bytes);
            let inputs = [vec![0x31; length], vec![0x72; 3], vec![0x19; bytes - 3]];
            for input in &inputs {
                sender.enqueue(input).unwrap();
            }
            let mut decoded = Vec::new();
            for now in 1..=30 {
                let Some((kind, packet)) = send(&mut sender, now) else {
                    break;
                };
                assert!(matches!(
                    kind,
                    PublicationKind::Data {
                        retransmission: false,
                        ..
                    }
                ));
                receiver.receive(at(now), &packet).unwrap();
                decoded.extend(drain(&mut receiver));
            }
            assert_eq!(decoded, inputs, "PDU {bytes}, SDU {length}");
        }
    }
}

#[test]
fn sequence_wrap_keeps_monotonic_count_and_never_reuses_old_slots() {
    let mut sender = entity(12);
    let mut receiver = entity(12);
    for step in 0..u64::from(SN_MODULUS) + 3 {
        let now = step * 4 + 1;
        sender.enqueue(&[(step % 251) as u8]).unwrap();
        let (kind, packet) = send(&mut sender, now).unwrap();
        assert_eq!(
            kind,
            PublicationKind::Data {
                sequence: sn(step),
                count: step as u32,
                retransmission: false
            }
        );
        assert_eq!(receiver.receive_count(sn(step)), Ok(step as u32));
        receiver.receive(at(now), &packet).unwrap();
        assert_eq!(drain(&mut receiver), [vec![(step % 251) as u8]]);
        sender
            .receive(at(now + 1), &status(12, sn(step + 1), &[]))
            .unwrap();
    }
    assert_eq!(sender.outstanding_pdus(), 0);
}

#[test]
fn independent_entities_and_clock_epochs_cannot_share_transition_state() {
    let mut one = entity(12);
    let mut two = entity(12);
    one.initiate_reset(at(1)).unwrap();
    assert_eq!(two.state(), AmState::Ready);
    two.enqueue(b"peer").unwrap();
    send(&mut two, 1).unwrap();
    let foreign = ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        ..at(2)
    };
    assert_eq!(two.tick(foreign), Err(AmError::Time));
    assert_eq!(two.tick(at(0)), Err(AmError::Time));
    assert_eq!(two.state(), AmState::Ready);
    assert_eq!(one.state(), AmState::ResetPending);
}

#[test]
fn count_exhaustion_never_wraps_into_reused_cipher_counts() {
    let mut configuration = config(12);
    configuration.initial_tx_hfn = MAX_HFN;
    configuration.initial_rx_hfn = MAX_HFN;
    let mut sender = AmEntity::new(configuration, limits(), at(0)).unwrap();
    let mut receiver = AmEntity::new(configuration, limits(), at(0)).unwrap();
    for step in 0..u64::from(SN_MODULUS) {
        let now = step * 2 + 1;
        sender.enqueue(&[0x36]).unwrap();
        let (kind, packet) = send(&mut sender, now).unwrap();
        assert_eq!(
            kind,
            PublicationKind::Data {
                sequence: sn(step),
                count: (MAX_HFN << SN_BITS) | step as u32,
                retransmission: false
            }
        );
        receiver.receive(at(now), &packet).unwrap();
        drain(&mut receiver);
        sender
            .receive(at(now + 1), &status(12, sn(step + 1), &[]))
            .unwrap();
    }
    sender.enqueue(b"no count reuse").unwrap();
    assert_eq!(
        sender.publish_next(at(9000), |_| Ok::<_, ()>(())),
        Err(PublishError::Rlc(AmError::CountExhausted))
    );
    assert_eq!(receiver.receive_count(0), Err(AmError::CountExhausted));
    assert_eq!(
        receiver.receive(at(9001), &reset(12, false, false, MAX_HFN)),
        Err(AmError::CountExhausted)
    );
    assert_eq!(receiver.state(), AmState::Failed);
}

fn status_with_prefix(size: usize, prefix: &[(usize, u32)], ack: u16) -> Vec<u8> {
    let mut bytes = vec![0; size];
    let mut writer = super::status::Writer::new(&mut bytes);
    for &(width, value) in prefix {
        writer.write(width, value).unwrap();
    }
    writer.ack(ack).unwrap();
    bytes
}

fn next_status(entity: &mut AmEntity, now: u64) -> super::status::Status {
    for step in 0..8 {
        if let Some((PublicationKind::Status, bytes)) = send(entity, now + step) {
            return super::status::parse_status(&bytes).unwrap();
        }
    }
    panic!("no STATUS published")
}

#[test]
fn poll_sufi_requests_a_status_report_instead_of_rejecting_the_status() {
    let mut entity = entity(40);
    // TS 25.322 9.2.2.11.9: SUFI type 8 (POLL) with Poll_SN, then the terminating ACK.
    let bytes = status_with_prefix(40, &[(4, 8), (12, 5)], 0);
    assert_eq!(entity.receive(at(1), &bytes), Ok(AmReceive::StatusApplied));
    let report = next_status(&mut entity, 2);
    assert_eq!(report.acknowledge, Some(0));
}

#[test]
fn obsolete_move_receiving_window_is_acknowledged_with_zero_and_current_vr_r() {
    let mut entity = entity(40);
    // TS 25.322 9.2.2.11.6: SUFI type 6 (MRW), LENGTH 1, SN_MRW 0, NLENGTH 2.
    let bytes = status_with_prefix(40, &[(4, 6), (4, 1), (12, 0), (4, 2)], 0);
    assert_eq!(entity.receive(at(1), &bytes), Ok(AmReceive::StatusApplied));
    let report = next_status(&mut entity, 2);
    assert_eq!(report.move_window_ack, Some((0, 0)));
    assert_eq!(report.acknowledge, Some(0));
}

#[test]
fn move_receiving_window_advances_vr_r_and_acknowledges_with_nlength() {
    let mut entity = entity(40);
    let bytes = status_with_prefix(40, &[(4, 6), (4, 1), (12, 3), (4, 1)], 0);
    assert_eq!(entity.receive(at(1), &bytes), Ok(AmReceive::StatusApplied));
    let report = next_status(&mut entity, 2);
    assert_eq!(report.move_window_ack, Some((1, 3)));
    assert_eq!(report.acknowledge, Some(3));
    // A received MRW_ACK is ignored because no explicit discard is ever started here.
    let bytes = status_with_prefix(40, &[(4, 7), (4, 0), (12, 3)], 0);
    assert_eq!(entity.receive(at(3), &bytes), Ok(AmReceive::StatusApplied));
}
