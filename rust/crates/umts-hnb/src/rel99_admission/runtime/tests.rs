// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::at;
use umts_dsp::waveform::WaveformWindow;
use umts_types::time::SampleInstant;

#[test]
fn startup_keeps_pending_proof_but_replaces_loss_without_claiming_delivery() {
    let sample = |index| SampleInstant {
        epoch: at(0).epoch,
        index,
    };
    let window = |first, next| WaveformWindow {
        first_sample: sample(first),
        next_sample: sample(next),
        chip_origin_sample: sample(first),
    };
    let (reader, mut writer) = crate::tx_evidence::channel(sample(0)).unwrap();
    let original = reader.receipt(sample(0), sample(10), at(10)).unwrap();
    let replacement = reader.receipt(sample(10), sample(20), at(20)).unwrap();
    let mut first = None;
    retain_start_submission(&mut first, original.clone());
    retain_start_submission(&mut first, replacement.clone());
    assert_eq!(first.as_ref().unwrap().air_end(), at(10));
    assert!(!first.as_ref().unwrap().submitted());
    assert!(writer.lost(window(0, 10)));
    retain_start_submission(&mut first, replacement);
    assert_eq!(first.as_ref().unwrap().air_end(), at(20));
    assert!(!first.as_ref().unwrap().submitted());
    assert!(!original.submitted());
    assert!(writer.accepted(window(10, 20)));
    assert!(first.as_ref().unwrap().submitted());
    let future = reader.receipt(sample(20), sample(30), at(30)).unwrap();
    retain_start_submission(&mut first, future);
    assert_eq!(first.as_ref().unwrap().air_end(), at(20));
    assert!(first.as_ref().unwrap().submitted());
}
