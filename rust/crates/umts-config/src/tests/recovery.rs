// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn recovery_delay_order_is_checked_by_full_config_validation() {
    let mut config = base_cfg();
    config.runtime.recovery.backoff_ms = 2;
    config.runtime.recovery.maximum_backoff_ms = 1;
    assert!(config.validate().is_err());
    config.runtime.recovery.maximum_backoff_ms = 2;
    config.validate().unwrap();
}

#[test]
fn serving_recovery_defaults_are_canonical_and_restart_budget_can_be_disabled() {
    let config = base_cfg();
    let recovery = &config.runtime.recovery;
    assert_eq!(recovery.maximum_restarts, 3);
    assert_eq!(recovery.backoff_ms, 1000);
    assert_eq!(recovery.maximum_backoff_ms, 30000);
    recovery.validate_schema().unwrap();
    let disabled = RustServingRecoveryConfig {
        maximum_restarts: 0,
        ..Default::default()
    };
    disabled.validate_schema().unwrap();
    let roundtrip: Config = serde_yaml::from_str(&serde_yaml::to_string(&config).unwrap()).unwrap();
    assert_eq!(roundtrip.runtime.recovery.maximum_restarts, 3);
}

#[test]
fn serving_recovery_yaml_overrides_preserve_the_explicit_lifetime_budget() {
    let serving: RuntimeSettings = serde_yaml::from_str(
        "recovery:\n  maximum_restarts: 17\n  backoff_ms: 250\n  maximum_backoff_ms: 9000\n",
    )
    .unwrap();
    serving.validate_schema().unwrap();
    assert_eq!(serving.recovery.maximum_restarts, 17);
    assert_eq!(serving.recovery.backoff_ms, 250);
    assert_eq!(serving.recovery.maximum_backoff_ms, 9000);
    let mut value = serde_json::to_value(base_cfg()).unwrap();
    value["runtime"]["recovery"] = serde_json::to_value(&serving.recovery).unwrap();
    let config: Config = serde_json::from_value(value).unwrap();
    config.validate().unwrap();
    assert_eq!(config.runtime.recovery.maximum_restarts, 17);
}

#[test]
fn serving_recovery_rejects_duplicate_unknown_and_implicit_reset_settings() {
    for yaml in [
        "recovery:\n  maximum_restarts: 1\n  maximum_restarts: 2\n",
        "recovery:\n  backoff_ms: 1\n  backoff_ms: 2\n",
        "recovery:\n  maximum_backoff_ms: 1\n  maximum_backoff_ms: 2\n",
        "recovery:\n  unknown_setting: true\n",
        "recovery:\n  stable_reset_ms: 1000\n",
        "recovery: {}\nrecovery: {}\n",
    ] {
        assert!(
            serde_yaml::from_str::<RuntimeSettings>(yaml).is_err(),
            "{yaml}"
        );
    }
}

#[test]
fn serving_recovery_validates_each_inclusive_scalar_bound() {
    for (field, minimum, maximum) in [
        ("maximum_restarts", 0, 100),
        ("backoff_ms", 1, 60000),
        ("maximum_backoff_ms", 1, 300000),
    ] {
        for valid in [minimum, maximum] {
            let mut value = serde_json::to_value(RustServingRecoveryConfig::default()).unwrap();
            value[field] = valid.into();
            let config: RustServingRecoveryConfig = serde_json::from_value(value).unwrap();
            config.validate_schema().unwrap();
        }
        for invalid in [minimum - 1, maximum + 1] {
            let mut value = serde_json::to_value(RustServingRecoveryConfig::default()).unwrap();
            value[field] = invalid.into();
            match serde_json::from_value::<RustServingRecoveryConfig>(value) {
                Ok(config) => assert!(config.validate_schema().is_err(), "{field}={invalid}"),
                Err(_) => assert!(invalid < 0),
            }
        }
    }
}
