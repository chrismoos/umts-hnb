// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    hsdpa_rx::{EvidenceLimits, FeedbackReceiver, Observation},
    test_support::at,
};
use std::{num::NonZeroUsize, sync::Arc};
use umts_dsp::{
    dedicated::UplinkDpchCode, hsdpa::FeedbackEvidence, uplink_scrambling::UplinkScramblingCode,
};
use umts_types::{RadioEpoch, UeKey, time::CHIPS_PER_SLOT};

const MAXIMUM_AGE: NonZeroU64 =
    NonZeroU64::new(20 * umts_types::time::CHIPS_PER_SECOND / 1000).unwrap();

fn policy() -> FeedbackPolicy {
    let limits = EvidenceLimits::new(0.5, 0.1, 0.5, 1.0, 0.01).unwrap();
    FeedbackPolicy {
        acknowledgement: limits,
        cqi: limits,
    }
}

fn measured(value: u8, correlation: f64) -> Evidence<Cqi> {
    Evidence::Measured(Observation {
        evidence: FeedbackEvidence {
            candidate: Some(Cqi::new(value).unwrap()),
            normalized_correlation: correlation,
            normalized_margin: 1.0,
            mean_soft_power: 1.0,
        },
        minimum_pilot_fraction: 1.0,
        mean_relative_pilot_disturbance: 0.0,
    })
}

fn report(start: ChipInstant, evidence: Evidence<Cqi>) -> Report {
    let code = Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(0).unwrap()).unwrap());
    let mut receiver = FeedbackReceiver::new(
        UeKey {
            epoch: start.epoch,
            slot: 0,
            generation: NonZeroU64::MIN,
        },
        code,
        1,
        start,
    )
    .unwrap();
    let mut port = receiver
        .connect(NonZeroUsize::MIN, NonZeroUsize::MIN)
        .unwrap();
    let request = Request::Cqi {
        start,
        repetitions: 1,
    };
    Report {
        id: port.request(request).unwrap(),
        request,
        result: Ok(Measurement::Cqi(evidence)),
        processed_until: None,
    }
}

#[test]
fn erasures_preserve_only_the_original_fresh_measurement() {
    let mut quality = Quality::default();
    let policy = policy();
    assert_eq!(
        quality.observe(&report(at(0), measured(30, 1.0)), &policy),
        Cqi::new(30).ok()
    );
    assert_eq!(
        quality.observe(&report(at(CHIPS_PER_SLOT), Evidence::Missing), &policy),
        None
    );
    assert_eq!(
        quality.observe(&report(at(2 * CHIPS_PER_SLOT), measured(29, 0.0)), &policy),
        None
    );
    let last_fresh = CHIPS_PER_SLOT + MAXIMUM_AGE.get();
    assert_eq!(
        quality.value(at(last_fresh), MAXIMUM_AGE),
        Cqi::new(30).ok()
    );
    assert_eq!(quality.value(at(last_fresh + 1), MAXIMUM_AGE), None);
    assert_eq!(quality.value(at(CHIPS_PER_SLOT - 1), MAXIMUM_AGE), None);
    let other_epoch = ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        index: CHIPS_PER_SLOT,
    };
    assert_eq!(quality.value(other_epoch, MAXIMUM_AGE), None);
}

#[test]
fn out_of_range_cqi_replaces_previous_quality_and_survives_erasure() {
    let mut quality = Quality::default();
    let policy = policy();
    quality.observe(&report(at(0), measured(30, 1.0)), &policy);
    assert_eq!(
        quality.observe(&report(at(CHIPS_PER_SLOT), measured(0, 1.0)), &policy),
        Cqi::new(0).ok()
    );
    quality.observe(&report(at(2 * CHIPS_PER_SLOT), Evidence::Missing), &policy);
    assert_eq!(
        quality.value(at(3 * CHIPS_PER_SLOT), MAXIMUM_AGE),
        Cqi::new(0).ok()
    );
}

#[test]
fn rejected_reports_advance_ordering_without_creating_quality() {
    let mut quality = Quality::default();
    let policy = policy();
    quality.observe(&report(at(2 * CHIPS_PER_SLOT), Evidence::Missing), &policy);
    assert_eq!(
        quality.observe(&report(at(CHIPS_PER_SLOT), measured(30, 1.0)), &policy),
        None
    );
    assert_eq!(
        quality.observe(&report(at(2 * CHIPS_PER_SLOT), measured(30, 1.0)), &policy),
        None
    );
    assert_eq!(quality.value(at(3 * CHIPS_PER_SLOT), MAXIMUM_AGE), None);
    assert_eq!(
        quality.observe(&report(at(3 * CHIPS_PER_SLOT), measured(28, 1.0)), &policy),
        Cqi::new(28).ok()
    );
    assert_eq!(
        quality.value(at(4 * CHIPS_PER_SLOT), MAXIMUM_AGE),
        Cqi::new(28).ok()
    );
}

#[test]
fn new_secondary_trial_rejects_old_cqi_and_accepts_fresh_recovery() {
    let mut quality = Quality::default();
    let policy = policy();
    assert_eq!(
        quality.observe(&report(at(0), measured(0, 1.0)), &policy),
        Cqi::new(0).ok()
    );
    let boundary = at(10 * CHIPS_PER_SLOT);
    quality.begin_trial(boundary);
    assert_eq!(quality.value(boundary, MAXIMUM_AGE), None);
    // Even a delayed good report ending exactly at the trial boundary is old.
    assert_eq!(
        quality.observe(&report(at(9 * CHIPS_PER_SLOT), measured(30, 1.0)), &policy),
        None
    );
    let other_epoch = ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        ..boundary
    };
    assert_eq!(
        quality.observe(&report(other_epoch, measured(30, 1.0)), &policy),
        None
    );
    assert_eq!(
        quality.observe(&report(boundary, measured(20, 1.0)), &policy),
        Cqi::new(20).ok()
    );
    assert_eq!(
        quality.value(at(11 * CHIPS_PER_SLOT), MAXIMUM_AGE),
        Cqi::new(20).ok()
    );
}
