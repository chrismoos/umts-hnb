// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::at;

fn sample(index: u64) -> SampleInstant {
    SampleInstant {
        epoch: at(0).epoch,
        index,
    }
}
fn window(first: u64, next: u64) -> WaveformWindow {
    WaveformWindow {
        first_sample: sample(first),
        next_sample: sample(next),
        chip_origin_sample: sample(first + 8),
    }
}

#[test]
fn only_a_contiguous_accepted_prefix_can_satisfy_immutable_transport_receipts() {
    let (reader, mut writer) = channel(sample(10)).unwrap();
    let receipt = reader.receipt(sample(10), sample(30), at(30)).unwrap();
    assert!(!receipt.submitted());
    assert!(writer.accepted(window(10, 20)));
    assert!(!receipt.submitted());
    assert!(writer.accepted(window(20, 30)));
    assert!(receipt.submitted());
    assert!(receipt.clone().submitted());
    let observed = receipt.snapshot();
    assert_eq!(observed.first, sample(10));
    assert_eq!(observed.next, sample(30));
    assert_eq!(observed.stream_first, sample(10));
    assert_eq!(observed.resolved_until, Some(sample(30)));
    assert_eq!(observed.intersects_loss, Some(false));
    assert_eq!(observed.emission, None);
}

#[test]
fn another_pipeline_with_the_same_epoch_and_timestamps_cannot_confirm_a_receipt() {
    let (reader, _writer) = channel(sample(0)).unwrap();
    let (other, mut other_writer) = channel(sample(0)).unwrap();
    let original = reader.receipt(sample(0), sample(10), at(10)).unwrap();
    let independent = other.receipt(sample(0), sample(10), at(10)).unwrap();
    assert!(other_writer.accepted(window(0, 10)));
    assert!(independent.submitted());
    assert!(!original.submitted());
}

#[test]
fn gap_overlap_failure_or_writer_drop_invalidates_observation_instead_of_inventing_progress() {
    for (first, next) in [(11, 20), (0, 20), (10, 10), (10, u64::MAX)] {
        let (reader, mut writer) = channel(sample(0)).unwrap();
        let receipt = reader.receipt(sample(0), sample(10), at(10)).unwrap();
        assert!(writer.accepted(window(0, 10)));
        assert!(receipt.submitted());
        assert!(!writer.accepted(window(first, next)));
        assert!(!receipt.submitted());
        assert!(!writer.accepted(window(10, 20)));
    }
    let (reader, mut writer) = channel(sample(0)).unwrap();
    let receipt = reader.receipt(sample(0), sample(10), at(10)).unwrap();
    assert!(writer.accepted(window(0, 10)));
    drop(writer);
    assert!(!receipt.submitted());
}

#[test]
fn receipt_admission_rejects_foreign_epochs_empty_ranges_and_sentinel_overflow() {
    let (reader, _writer) = channel(sample(10)).unwrap();
    assert!(reader.receipt(sample(0), sample(20), at(20)).is_none());
    assert!(reader.receipt(sample(10), sample(10), at(10)).is_none());
    assert!(
        reader
            .receipt(sample(10), sample(u64::MAX), at(20))
            .is_none()
    );
    let mut foreign = sample(20);
    foreign.epoch = umts_types::RadioEpoch::new(std::num::NonZeroU64::new(2).unwrap());
    assert!(reader.receipt(sample(10), foreign, at(20)).is_none());
    assert!(channel(sample(u64::MAX)).is_none());
}

#[test]
fn explicit_loss_resolves_custody_but_cannot_confirm_delivery_or_poison_future_transports() {
    let (reader, mut writer) = channel(sample(0)).unwrap();
    let accepted = reader.receipt(sample(0), sample(10), at(10)).unwrap();
    let lost = reader.receipt(sample(10), sample(20), at(20)).unwrap();
    let spanning = reader.receipt(sample(0), sample(30), at(30)).unwrap();
    let future = reader.receipt(sample(20), sample(30), at(30)).unwrap();
    let resolution = spanning.resolution();
    assert!(writer.accepted(window(0, 10)));
    assert!(writer.lost(window(10, 20)));
    assert!(accepted.submitted());
    assert!(!lost.viable());
    assert!(!lost.submitted());
    assert!(!spanning.viable());
    assert!(!resolution.resolved());
    assert!(future.viable());
    assert!(writer.accepted(window(20, 30)));
    assert!(future.submitted());
    assert!(!spanning.submitted());
    let observed = spanning.snapshot();
    assert_eq!(observed.resolved_until, Some(sample(30)));
    assert_eq!(observed.intersects_loss, Some(true));
    assert!(resolution.viable());
    assert!(resolution.resolved());
}

#[test]
fn bounded_loss_history_never_turns_an_evicted_loss_into_delivery() {
    let (reader, mut writer) = channel(sample(0)).unwrap();
    let original = reader.receipt(sample(0), sample(1), at(1)).unwrap();
    for index in 0..1024 {
        let first = index * 2;
        assert!(writer.lost(window(first, first + 1)));
        assert!(writer.accepted(window(first + 1, first + 2)));
        assert!(!original.submitted());
    }
    assert!(!original.viable());
    assert!(original.resolution().resolved());
    let future = reader
        .receipt(sample(2048), sample(2050), at(2050))
        .unwrap();
    assert!(writer.accepted(window(2048, 2050)));
    assert!(future.submitted());
}
