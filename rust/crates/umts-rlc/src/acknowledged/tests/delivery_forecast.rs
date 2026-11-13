// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn emitted_data_pdus(sender: &mut AmEntity, start: u64) -> usize {
    let mut count = 0;
    for offset in 0..512 {
        if !sender.pending_new_data() {
            break;
        }
        let Some((kind, _)) = send(sender, start + offset) else {
            break;
        };
        assert!(matches!(kind, PublicationKind::Data { .. }));
        count += 1;
    }
    assert!(!sender.pending_new_data());
    count
}

#[test]
fn forecast_matches_single_sdu_emission_and_preserves_state() {
    let mut configuration = config(16);
    configuration.transmit_window = NonZeroU16::new(512).unwrap();
    let mut sender = AmEntity::new(configuration, limits(), at(0)).unwrap();
    let before = sender.window_positions();
    let forecast = sender.forecast_delivery(23).unwrap();
    assert_eq!(forecast.queued_pdus, 0);
    assert_eq!(forecast.first_position, before.next_transmit);
    assert_eq!(
        forecast.next_position,
        before.next_transmit + forecast.total_new_pdus as u64
    );
    assert_eq!(forecast.outstanding_pdus, 0);
    assert_eq!(sender.window_positions(), before);
    sender.enqueue(&[0x5a; 23]).unwrap();
    assert_eq!(emitted_data_pdus(&mut sender, 1), forecast.total_new_pdus);
}

#[test]
fn forecast_covers_queued_sdus_and_an_appended_sdu_without_mutation() {
    let mut configuration = config(16);
    configuration.transmit_window = NonZeroU16::new(512).unwrap();
    let mut sender = AmEntity::new(configuration, limits(), at(0)).unwrap();
    sender.enqueue(&[0x11; 7]).unwrap();
    sender.enqueue(&[0x22; 19]).unwrap();
    let before = sender.window_positions();
    let forecast = sender.forecast_delivery(13).unwrap();
    assert!(forecast.queued_pdus > 0);
    assert!(forecast.total_new_pdus >= forecast.queued_pdus);
    sender.enqueue(&[0x33; 13]).unwrap();
    assert_eq!(sender.window_positions(), before);
    assert_eq!(emitted_data_pdus(&mut sender, 1), forecast.total_new_pdus);
}

#[test]
fn forecast_accounts_for_a_partial_head_and_final_li_delimiter() {
    let mut configuration = config(16);
    configuration.transmit_window = NonZeroU16::new(512).unwrap();
    let mut sender = AmEntity::new(configuration, limits(), at(0)).unwrap();
    sender.enqueue(&[0x44; 31]).unwrap();
    send(&mut sender, 1).unwrap();
    assert!(sender.pending_new_data());
    sender.enqueue(&[0x55; 9]).unwrap();
    let forecast = sender.forecast_delivery(5).unwrap();
    sender.enqueue(&[0x66; 5]).unwrap();
    assert_eq!(emitted_data_pdus(&mut sender, 2), forecast.total_new_pdus);
}

#[test]
fn forecast_counts_exact_boundary_delimiters_for_both_li_widths() {
    for (pdu_bytes, length) in [(16, 14), (160, 157)] {
        let mut configuration = config(pdu_bytes);
        configuration.transmit_window = NonZeroU16::new(512).unwrap();
        let mut sender = AmEntity::new(configuration, limits(), at(0)).unwrap();
        let forecast = sender.forecast_delivery(length).unwrap();
        assert_eq!(forecast.total_new_pdus, 2, "PDU bytes {pdu_bytes}");
        sender.enqueue(&[0x77; 157][..length]).unwrap();
        assert_eq!(emitted_data_pdus(&mut sender, 1), 2);
    }
}

#[test]
fn forecast_rejects_reset_failed_and_full_unacknowledged_window() {
    let mut reset = entity(16);
    reset.initiate_reset(at(1)).unwrap();
    assert_eq!(reset.forecast_delivery(1), Err(AmError::State));

    let mut configuration = config(16);
    configuration.transmit_window = NonZeroU16::MIN;
    let mut full = AmEntity::new(configuration, limits(), at(0)).unwrap();
    full.enqueue(b"one").unwrap();
    send(&mut full, 1).unwrap();
    assert_eq!(full.forecast_delivery(1), Err(AmError::Capacity));

    let mut failed_configuration = config(16);
    failed_configuration.maximum_data_attempts = NonZeroU16::MIN;
    failed_configuration.maximum_reset_attempts = NonZeroU16::MIN;
    let mut failed = AmEntity::new(failed_configuration, limits(), at(0)).unwrap();
    failed.enqueue(b"failed").unwrap();
    assert!(
        failed
            .publish_next(at(1), |_| -> Result<(), ()> { panic!("no data") })
            .is_ok()
    );
    assert_eq!(
        failed.publish_next(at(2), |_| -> Result<(), ()> { panic!("no reset") }),
        Err(PublishError::Rlc(AmError::ResetLimit))
    );
    assert_eq!(failed.forecast_delivery(1), Err(AmError::State));
}

#[test]
fn forecast_reports_a_monotonic_queue_revision_and_outstanding_pdus() {
    let mut sender = entity(16);
    let initial = sender.forecast_delivery(1).unwrap();
    sender.enqueue(b"queued").unwrap();
    let queued = sender.forecast_delivery(1).unwrap();
    assert!(queued.queue_revision > initial.queue_revision);
    send(&mut sender, 1).unwrap();
    let after = sender.forecast_delivery(1).unwrap();
    assert!(after.queue_revision > queued.queue_revision);
    assert_eq!(after.outstanding_pdus, 1);
}
