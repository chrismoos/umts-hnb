// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn config() -> Config {
    serde_yaml::from_str(include_str!(
        "../../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .unwrap()
}

#[test]
fn cancelled_open_does_not_touch_radio_or_network() {
    let config = super::super::tests::config();
    let prepared = PreparedServing::new(&config, super::super::tests::epoch()).unwrap();
    let error = match prepared.open_with_cancellation(|| true) {
        Ok(_) => panic!("canceled startup must not open hardware"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "serving startup canceled");
}

#[test]
fn restart_budget_uses_canonical_defaults_and_allows_explicit_disable() {
    let mut config = config();
    let budget = RecoveryBudget::new(&config).unwrap();
    assert_eq!(budget.maximum_restarts, 3);
    assert_eq!(budget.attempts(), 0);
    assert_eq!(budget.backoff, Duration::from_secs(1));
    assert_eq!(budget.maximum_backoff, Duration::from_secs(30));
    config.runtime.recovery.maximum_restarts = 0;
    let budget = RecoveryBudget::new(&config).unwrap();
    assert_eq!(budget.maximum_restarts, 0);
    assert_eq!(budget.attempts(), 0);
}

#[test]
fn invalid_restart_delay_order_and_schema_bounds_are_rejected_before_open() {
    let mut config = config();
    config.runtime.recovery.backoff_ms = 2;
    config.runtime.recovery.maximum_backoff_ms = 1;
    assert!(RecoveryBudget::new(&config).is_err());
    config.runtime.recovery.maximum_backoff_ms = 2;
    assert!(RecoveryBudget::new(&config).is_ok());
    config.runtime.recovery.maximum_restarts = 101;
    assert!(RecoveryBudget::new(&config).is_err());
}

#[test]
fn exponential_backoff_is_capped_without_shift_or_duration_overflow() {
    let base = Duration::from_millis(1000);
    let maximum = Duration::from_millis(30_000);
    for (attempt, seconds) in [
        (0, 1),
        (1, 2),
        (2, 4),
        (3, 8),
        (4, 16),
        (5, 30),
        (31, 30),
        (32, 30),
        (99, 30),
        (u16::MAX, 30),
    ] {
        assert_eq!(
            retry_delay(base, maximum, attempt),
            Duration::from_secs(seconds)
        );
    }
    assert_eq!(
        retry_delay(Duration::MAX, Duration::MAX, u16::MAX),
        Duration::MAX
    );
    assert_eq!(retry_delay(base, base, 0), base);
}

#[test]
fn core_retry_is_unlimited_by_default_and_resets_on_connection() {
    let mut config = config();
    config.core.iuh.retry_attempts = 0;
    config.core.iuh.retry_backoff_secs = 5;
    let mut retry = CoreRetry::new(&config);
    for attempt in 1..=1000 {
        assert_eq!(retry.failed(), Some(Duration::from_secs(5)));
        assert_eq!(retry.failures(), attempt);
    }
    retry.connected();
    assert_eq!(retry.failures(), 0);
}

#[test]
fn core_retry_limit_counts_consecutive_failures() {
    let mut config = config();
    config.core.iuh.retry_attempts = 2;
    config.core.iuh.retry_backoff_secs = 1;
    let mut retry = CoreRetry::new(&config);
    assert!(retry.failed().is_some());
    assert!(retry.failed().is_some());
    assert_eq!(retry.failed(), None);
    retry.connected();
    assert!(retry.failed().is_some());
}
