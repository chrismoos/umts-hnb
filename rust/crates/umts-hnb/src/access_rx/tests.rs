// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::at;
use umts_dsp::{
    prach::{PrachCode, PreambleSignature},
    receive_filter::FilteredWindow,
};

fn view(
    first_chip: u64,
    chips: &[Complex32],
    generation: u64,
    discontinuity: bool,
) -> FilteredRx<'_> {
    FilteredRx {
        window: FilteredWindow {
            first_center: (!chips.is_empty()).then_some(SampleInstant {
                epoch: at(0).epoch,
                index: first_chip * 2,
            }),
            sample_stride: 2,
            next_input: SampleInstant {
                epoch: at(0).epoch,
                index: (first_chip + chips.len() as u64) * 2 + 32,
            },
        },
        generation,
        discontinuity,
        chips,
    }
}

fn receiver() -> (PrachWindowReceiver, usize) {
    let plan = Arc::new(
        PreambleSearchPlan::new(PrachCode::new(7504).unwrap(), u16::MAX, 0..4096, 33).unwrap(),
    );
    let length = plan.input_length();
    (PrachWindowReceiver::new(plan, SamplesPerChip::Two), length)
}

#[test]
fn split_filtered_chunks_form_one_identified_search_window_without_losing_prefixes() {
    let (mut receiver, length) = receiver();
    receiver.arm(at(1000)).unwrap();
    let mut chips = vec![Complex32::default(); 100 + length + 31];
    let code = PrachCode::new(7504).unwrap();
    let signature = PreambleSignature::new(13).unwrap();
    code.write_preamble(signature, &mut chips[117..117 + 4096])
        .unwrap();
    let mut offset = 0;
    let mut result = None;
    for size in [1, 7, 23, 100, 997, 1300, 1831] {
        if let Some(window) = receiver
            .feed(view(
                900 + offset as u64,
                &chips[offset..offset + size],
                1,
                false,
            ))
            .unwrap()
        {
            result = window.strongest(signature);
        }
        offset += size;
    }
    assert_eq!(offset, chips.len());
    let result = result.unwrap();
    assert_eq!(result.code, code);
    assert_eq!(result.correlation_start, at(1017));
    assert!(result.coherent_fraction > 0.99999);
    assert_eq!(
        receiver.feed(view(6000, &[], 1, false)).err(),
        Some(AccessRxError::NotArmed)
    );
}

#[test]
fn epoch_phase_missing_data_and_generation_changes_discard_the_entire_window() {
    let (mut receiver, _) = receiver();
    let chips = vec![Complex32::default(); 30];
    receiver.arm(at(1000)).unwrap();
    assert!(
        receiver
            .feed(view(1000, &chips, 1, false))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        receiver.feed(view(1030, &chips, 2, true)).err(),
        Some(AccessRxError::Gap)
    );
    assert_eq!(
        receiver.feed(view(1060, &chips, 2, false)).err(),
        Some(AccessRxError::NotArmed)
    );
    receiver.arm(at(1000)).unwrap();
    assert_eq!(
        receiver.feed(view(1001, &chips, 3, false)).err(),
        Some(AccessRxError::Gap)
    );
    receiver.arm(at(1000)).unwrap();
    let mut input = view(1000, &chips, 3, false);
    input.window.first_center.as_mut().unwrap().index += 1;
    assert_eq!(receiver.feed(input).err(), Some(AccessRxError::Phase));
    receiver.arm(at(1000)).unwrap();
    let mut input = view(1000, &chips, 3, false);
    input.window.sample_stride = 4;
    assert_eq!(receiver.feed(input).err(), Some(AccessRxError::Phase));
    receiver.arm(at(1000)).unwrap();
    let mut input = view(1000, &chips, 3, false);
    input.window.first_center.as_mut().unwrap().epoch =
        umts_types::RadioEpoch::new(std::num::NonZeroU64::new(2).unwrap());
    assert_eq!(receiver.feed(input).err(), Some(AccessRxError::Epoch));
    assert_eq!(receiver.arm(at(u64::MAX - 1)), Err(AccessRxError::Time));
}

#[test]
fn invalid_evidence_never_publishes_a_candidate_and_a_fresh_arm_recovers() {
    let (mut receiver, length) = receiver();
    receiver.arm(at(1000)).unwrap();
    let mut chips = vec![Complex32::default(); length];
    chips[99].re = f32::NAN;
    assert_eq!(
        receiver.feed(view(1000, &chips, 1, false)).err(),
        Some(AccessRxError::Dsp(DspError::NonFiniteEvidence))
    );
    chips[99].re = 0.0;
    receiver.arm(at(2000)).unwrap();
    let signature = PreambleSignature::new(3).unwrap();
    PrachCode::new(7504)
        .unwrap()
        .write_preamble(signature, &mut chips[7..7 + 4096])
        .unwrap();
    let window = receiver
        .feed(view(2000, &chips, 2, false))
        .unwrap()
        .unwrap();
    assert_eq!(
        window.strongest(signature).unwrap().correlation_start,
        at(2007)
    );
}
