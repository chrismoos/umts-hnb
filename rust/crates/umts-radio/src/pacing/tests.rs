// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn budget() -> LeadBudget {
    LeadBudget::new(
        LeadDurations {
            minimum_submission: Duration::from_micros(100),
            maximum_submission: Duration::from_millis(2),
            maximum_preparation: Duration::from_millis(4),
        },
        NonZeroU64::new(1_000_000).unwrap(),
        NonZeroU64::new(667).unwrap(),
    )
    .unwrap()
}

#[test]
fn submission_and_preparation_bound_the_complete_window_with_inclusive_deadlines() {
    let budget = budget();
    assert_eq!(budget.maximum_preparation_ticks(), 4000);
    assert_eq!(budget.preparation(1000, 3000, 3667), Ok(Gate::Ready));
    assert_eq!(
        budget.submission(1000, 3000, 3667),
        Ok(Gate::WaitUntil(1667))
    );
    assert_eq!(budget.submission(1667, 3000, 3667), Ok(Gate::Ready));
    assert_eq!(budget.submission(2900, 3000, 3667), Ok(Gate::Ready));
    assert_eq!(
        budget.submission(2901, 3000, 3667),
        Ok(Gate::Missed { shortfall_ticks: 1 })
    );
    assert_eq!(budget.preparation(0, 4000, 4667), Ok(Gate::WaitUntil(667)));
    for now in 0..3000_u64 {
        let expected = if now > 2900 {
            Gate::Missed {
                shortfall_ticks: u128::from(now - 2900),
            }
        } else if now < 1667 {
            Gate::WaitUntil(1667)
        } else {
            Gate::Ready
        };
        assert_eq!(budget.submission(now, 3000, 3667), Ok(expected));
    }
}

#[test]
fn uncertainty_uses_the_earliest_time_for_the_complete_window_horizon() {
    let budget = budget();
    assert_eq!(budget.submission(2000, 3000, 3667), Ok(Gate::Ready));
    assert_eq!(
        budget.submission_bounds(
            ClockBounds {
                earliest: 1666,
                latest: 2000
            },
            3000,
            3667
        ),
        Ok(Gate::WaitUntil(1667))
    );
    assert_eq!(
        budget.submission_bounds(
            ClockBounds {
                earliest: 1667,
                latest: 2000
            },
            3000,
            3667
        ),
        Ok(Gate::Ready)
    );
    assert_eq!(budget.preparation(1000, 4000, 4667), Ok(Gate::Ready));
    assert_eq!(
        budget.preparation_bounds(
            ClockBounds {
                earliest: 666,
                latest: 1000
            },
            4000,
            4667
        ),
        Ok(Gate::WaitUntil(667))
    );
    assert_eq!(
        budget.preparation_bounds(
            ClockBounds {
                earliest: 667,
                latest: 1000
            },
            4000,
            4667
        ),
        Ok(Gate::Ready)
    );
}

#[test]
fn uncertainty_uses_the_latest_time_for_minimum_lead_and_rejects_ambiguous_windows() {
    let budget = budget();
    assert_eq!(
        budget.submission_bounds(
            ClockBounds {
                earliest: 2000,
                latest: 2900
            },
            3000,
            3667
        ),
        Ok(Gate::Ready)
    );
    for earliest in [1000, 2000, 2901] {
        let now = ClockBounds {
            earliest,
            latest: 2901,
        };
        assert_eq!(
            budget.submission_bounds(now, 3000, 3667),
            Ok(Gate::Missed { shortfall_ticks: 1 })
        );
        assert_eq!(
            budget.preparation_bounds(now, 3000, 3667),
            Ok(Gate::Missed { shortfall_ticks: 1 })
        );
    }
    let now = ClockBounds {
        earliest: u64::MAX - 1000,
        latest: u64::MAX,
    };
    assert_eq!(
        budget.submission_bounds(now, u64::MAX - 5, u64::MAX),
        Ok(Gate::Missed {
            shortfall_ticks: 105
        })
    );
    assert_eq!(
        budget.preparation_bounds(now, u64::MAX - 5, u64::MAX),
        Ok(Gate::Missed {
            shortfall_ticks: 105
        })
    );
}

#[test]
fn reversed_clock_bounds_are_invalid_in_both_gates() {
    let budget = budget();
    for now in [
        ClockBounds {
            earliest: 2,
            latest: 1,
        },
        ClockBounds {
            earliest: u64::MAX,
            latest: 0,
        },
    ] {
        assert_eq!(
            budget.submission_bounds(now, 3000, 3667),
            Err(LeadError::InvalidWindow)
        );
        assert_eq!(
            budget.preparation_bounds(now, 3000, 3667),
            Err(LeadError::InvalidWindow)
        );
    }
}

#[test]
fn equal_clock_bounds_preserve_point_time_deadlines_and_quantization() {
    let budget = budget();
    for (now, expected) in [
        (0, Gate::WaitUntil(1667)),
        (1666, Gate::WaitUntil(1667)),
        (1667, Gate::Ready),
        (2900, Gate::Ready),
        (2901, Gate::Missed { shortfall_ticks: 1 }),
    ] {
        let bounds = ClockBounds {
            earliest: now,
            latest: now,
        };
        assert_eq!(budget.submission_bounds(bounds, 3000, 3667), Ok(expected));
        assert_eq!(
            budget.submission_bounds(bounds, 3000, 3667),
            budget.submission(now, 3000, 3667)
        );
        assert_eq!(
            budget.preparation_bounds(bounds, 3000, 3667),
            budget.preparation(now, 3000, 3667)
        );
    }
    let quantized = LeadBudget::new(
        LeadDurations {
            minimum_submission: Duration::from_nanos(1),
            maximum_submission: Duration::from_secs(1),
            maximum_preparation: Duration::from_secs(2),
        },
        NonZeroU64::new(3).unwrap(),
        NonZeroU64::MIN,
    )
    .unwrap();
    let origin = ClockBounds {
        earliest: 0,
        latest: 0,
    };
    assert_eq!(quantized.submission_bounds(origin, 1, 3), Ok(Gate::Ready));
    assert_eq!(
        quantized.submission_bounds(origin, 2, 4),
        Ok(Gate::WaitUntil(1))
    );
    assert_eq!(quantized.preparation_bounds(origin, 4, 6), Ok(Gate::Ready));
}

#[test]
fn quantization_rounds_minimum_up_and_maximum_down_for_the_actual_tick_rate() {
    let budget = LeadBudget::new(
        LeadDurations {
            minimum_submission: Duration::from_nanos(1),
            maximum_submission: Duration::from_secs(1),
            maximum_preparation: Duration::from_secs(2),
        },
        NonZeroU64::new(3).unwrap(),
        NonZeroU64::MIN,
    )
    .unwrap();
    assert_eq!(budget.minimum_ticks(), 1);
    assert_eq!(budget.maximum_preparation_ticks(), 6);
    assert_eq!(budget.submission(0, 1, 3), Ok(Gate::Ready));
    assert_eq!(budget.submission(0, 2, 4), Ok(Gate::WaitUntil(1)));
    assert_eq!(budget.preparation(0, 4, 6), Ok(Gate::Ready));
}

#[test]
fn exhausted_device_time_and_invalid_windows_do_not_overflow_or_wrap() {
    let budget = budget();
    assert_eq!(
        budget.submission(u64::MAX - 20, u64::MAX - 5, u64::MAX),
        Ok(Gate::Missed {
            shortfall_ticks: 85
        })
    );
    assert_eq!(
        budget.submission(u64::MAX - 1000, u64::MAX - 500, u64::MAX),
        Ok(Gate::Ready)
    );
    assert_eq!(
        budget.submission(0, 100, 100),
        Err(LeadError::InvalidWindow)
    );
    assert_eq!(
        budget.submission(0, 100, 2001),
        Err(LeadError::InvalidWindow)
    );
}

#[test]
fn impossible_lead_budgets_and_duration_overflow_are_rejected_before_use() {
    for limits in [
        LeadDurations {
            minimum_submission: Duration::ZERO,
            maximum_submission: Duration::from_millis(2),
            maximum_preparation: Duration::from_millis(4),
        },
        LeadDurations {
            minimum_submission: Duration::from_millis(1),
            maximum_submission: Duration::from_millis(1),
            maximum_preparation: Duration::from_millis(4),
        },
        LeadDurations {
            minimum_submission: Duration::from_micros(100),
            maximum_submission: Duration::from_millis(2),
            maximum_preparation: Duration::from_millis(1),
        },
    ] {
        assert!(matches!(
            LeadBudget::new(
                limits,
                NonZeroU64::new(1_000_000).unwrap(),
                NonZeroU64::new(667).unwrap()
            ),
            Err(LeadError::InvalidBudget)
        ));
    }
    assert!(matches!(
        LeadBudget::new(
            LeadDurations {
                minimum_submission: Duration::MAX,
                maximum_submission: Duration::MAX,
                maximum_preparation: Duration::MAX,
            },
            NonZeroU64::new(u64::MAX).unwrap(),
            NonZeroU64::MIN
        ),
        Err(LeadError::Overflow)
    ));
}
