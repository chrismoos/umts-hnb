// SPDX-License-Identifier: AGPL-3.0-or-later

use std::time::Duration;

use anyhow::{Result, bail};
use umts_config::Config;
use umts_dsp::prach::PREAMBLE_CHIPS;
use umts_iuh::session::ControlTime;
use umts_rrc::random_access::AichTiming;
use umts_types::time::{CHIPS_PER_SECOND, CHIPS_PER_SLOT};

use crate::{
    epoch::EpochSettings,
    radio_io::{ClockSamplingSettings, TxHealthSettings},
};

const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const MAXIMUM_HEALTH_EVENTS: u64 = 3;
const SLOT_DURATION_US: u32 = 667;
pub(super) const MAXIMUM_ACCESS_DELAY_CHIPS: u64 = 256;
const MICROSECONDS_PER_SECOND: u64 = 1_000_000;

pub(super) fn validate(config: &Config) -> Result<()> {
    super::inactivity::Inactivity::new(config)?;
    super::hsupa::validate(config)?;
    if config.runtime.maximum_sdu_bytes as usize
        > umts_rrc_codec::MAX_DEDICATED_BITS / umts_types::bits::BITS_PER_BYTE
    {
        bail!("runtime.maximum_sdu_bytes exceeds the RRC codec limit");
    }
    validate_with_aich(config, AichTiming::FiveSlots)
}

pub(super) fn validate_cell(config: &Config, cell: &crate::cell::CellInformation) -> Result<()> {
    for prach in cell.sib5().prach() {
        validate_with_aich(config, prach.settings().aich.timing)?;
    }
    Ok(())
}

fn validate_with_aich(config: &Config, timing: AichTiming) -> Result<()> {
    config.validate_schema()?;

    let cpich_tx_dbm = config.cell.advertised_power.cpich_tx_dbm.ok_or_else(|| {
        anyhow::anyhow!("cell.advertised_power.cpich_tx_dbm is required when serving starts")
    })?;
    if !(-10..=50).contains(&cpich_tx_dbm) {
        bail!("cell.advertised_power.cpich_tx_dbm must be in -10..=50 dBm");
    }

    let uplink_interference_dbm = config
        .cell
        .advertised_power
        .uplink_interference_dbm
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cell.advertised_power.uplink_interference_dbm is required when serving starts"
            )
        })?;
    if !(-110..=-50).contains(&uplink_interference_dbm) {
        bail!("cell.advertised_power.uplink_interference_dbm must be in -110..=-50 dBm");
    }

    if !(config.power_control.downlink.cpich_amplitude > 0.0
        && config.power_control.downlink.cpich_amplitude <= 0.25)
    {
        bail!("power_control.downlink.cpich_amplitude must be greater than 0 and at most 0.25");
    }
    if config.runtime.minimum_submission_us > config.runtime.maximum_submission_us {
        bail!("runtime.minimum_submission_us must not exceed maximum_submission_us");
    }
    if config.runtime.maximum_submission_us > config.runtime.maximum_preparation_us {
        bail!("runtime.maximum_submission_us must not exceed maximum_preparation_us");
    }
    let minimum_submission_with_slot = config
        .runtime
        .minimum_submission_us
        .checked_add(SLOT_DURATION_US * u32::from(config.runtime.tx_batch_slots))
        .ok_or_else(|| anyhow::anyhow!("runtime.serving submission timing overflows"))?;
    if minimum_submission_with_slot > config.runtime.maximum_submission_us {
        bail!(
            "runtime.maximum_submission_us must cover minimum_submission_us plus a complete batch"
        );
    }
    let clock = clock_sampling(config)?;
    let drift_us = match config.runtime.clock.maximum_age_us {
        Some(age) => u64::from(age)
            .checked_mul(u64::from(config.runtime.clock.maximum_drift_ppm))
            .and_then(|value| value.checked_mul(2))
            .ok_or_else(|| anyhow::anyhow!("clock uncertainty overflows"))?
            .div_ceil(MICROSECONDS_PER_SECOND),
        // Without an expiry horizon, runtime clock bounds decide whether a safe submission window remains.
        None => 0,
    };
    let occupied_us = u64::from(minimum_submission_with_slot)
        + u64::from(config.runtime.clock.maximum_query_us)
        + drift_us
        + 1;
    if occupied_us >= u64::from(config.runtime.maximum_submission_us) {
        bail!("runtime.serving clock uncertainty leaves no safe submission window");
    }
    if clock
        .model
        .maximum_age
        .is_some_and(|age| clock.interval + clock.model.maximum_query >= age)
    {
        bail!("runtime.clock.maximum_age_us must exceed interval_us plus maximum_query_us");
    }
    let startup_lead_us = config
        .runtime
        .startup_lead_ms
        .checked_mul(1000)
        .ok_or_else(|| anyhow::anyhow!("runtime.serving startup timing overflows"))?;
    if startup_lead_us <= config.runtime.maximum_preparation_us {
        bail!("runtime.startup_lead_ms must exceed maximum_preparation_us");
    }
    let maximum_preparation_chips = u64::from(config.runtime.maximum_preparation_us)
        .checked_mul(CHIPS_PER_SECOND)
        .ok_or_else(|| anyhow::anyhow!("runtime.serving preparation timing overflows"))?
        .div_ceil(MICROSECONDS_PER_SECOND);
    // Aligned batches freeze AICH only at their first slot. TS 25.211 7.3 uses two-slot access intervals.
    let maximum_first_sample_lead_chips = maximum_preparation_chips
        .checked_sub(CHIPS_PER_SLOT * u64::from(config.runtime.tx_batch_slots))
        .ok_or_else(|| anyhow::anyhow!("runtime.serving preparation must cover a complete slot"))?;
    let timing_budget = maximum_first_sample_lead_chips
        .checked_add(CHIPS_PER_SLOT)
        .and_then(|value| value.checked_add(MAXIMUM_ACCESS_DELAY_CHIPS))
        .and_then(|value| value.checked_add(u64::from(config.receiver.filter_span_chips)))
        .ok_or_else(|| anyhow::anyhow!("runtime.serving radio timing overflows"))?;
    let available = u64::from(timing.preamble_to_indicator_chips()) - PREAMBLE_CHIPS as u64;
    if timing_budget >= available {
        bail!(
            "runtime.serving preparation, receive, access, and filter timing must remain below {available} chips"
        );
    }
    Ok(())
}

pub(super) fn clock_sampling(config: &Config) -> Result<ClockSamplingSettings> {
    let clock = &config.runtime.clock;
    clock.validate_schema()?;
    let settings = ClockSamplingSettings {
        interval: Duration::from_micros(u64::from(clock.interval_us)),
        model: umts_radio::clock_model::ClockSettings {
            maximum_age: clock
                .maximum_age_us
                .map(|age| Duration::from_micros(u64::from(age))),
            maximum_query: Duration::from_micros(u64::from(clock.maximum_query_us)),
            maximum_drift_ppm: clock.maximum_drift_ppm,
        },
    };
    if settings.interval.is_zero()
        || settings
            .model
            .maximum_age
            .is_some_and(|age| settings.interval >= age)
    {
        bail!("runtime.serving clock sampling interval must be below maximum age");
    }
    umts_radio::clock_model::ClockModel::new(std::num::NonZeroU64::MIN, settings.model)
        .map_err(|error| anyhow::anyhow!("invalid runtime.serving clock model: {error:?}"))?;
    Ok(settings)
}

pub(super) fn controller_idle_wait(config: &Config) -> Duration {
    Duration::from_micros(u64::from(
        config
            .runtime
            .controller_idle_wait_us
            .unwrap_or(config.runtime.idle_wait_us),
    ))
}

pub(super) fn epoch_settings(config: &Config) -> Result<EpochSettings> {
    validate(config)?;

    Ok(EpochSettings {
        idle_wait: Duration::from_micros(u64::from(config.runtime.idle_wait_us)),
        startup_timeout: Duration::from_millis(u64::from(config.runtime.startup_timeout_ms)),
        clock_stall_timeout: Duration::from_millis(u64::from(
            config.runtime.clock_stall_timeout_ms,
        )),
        control_origin: ControlTime(0),
        rx_activation_tick: None,
        health: TxHealthSettings {
            poll_interval: HEALTH_POLL_INTERVAL,
            maximum_underflows: MAXIMUM_HEALTH_EVENTS,
            maximum_late_packets: MAXIMUM_HEALTH_EVENTS,
            maximum_unknown_events: MAXIMUM_HEALTH_EVENTS,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let mut config: Config = serde_yaml::from_str(include_str!(
            "../../../../../rust/test-support/fixtures/config/rel99.yaml"
        ))
        .expect("reference profile parses");
        config.cell.advertised_power.cpich_tx_dbm = Some(30);
        config.cell.advertised_power.uplink_interference_dbm = Some(-90);
        config
    }

    #[test]
    fn clock_settings_fail_before_radio_open_when_bounds_cannot_fit() {
        let mut config = config();
        let settings = clock_sampling(&config).unwrap();
        assert_eq!(settings.interval, Duration::from_secs(1));
        assert_eq!(settings.model.maximum_age, None);
        assert_eq!(settings.model.maximum_query, Duration::from_micros(100));
        assert_eq!(settings.model.maximum_drift_ppm, 100);
        assert!(validate(&config).is_ok());
        config.runtime.clock.maximum_age_us = Some(1_000_000);
        assert!(validate(&config).is_err());
        config.runtime.clock.interval_us = 50_000;
        config.runtime.clock.maximum_age_us = Some(200_000);
        config.runtime.clock.maximum_query_us = 1000;
        assert!(validate(&config).is_err());
        config.runtime.clock.maximum_query_us = 100;
        config.runtime.clock.maximum_drift_ppm = 10_000;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn serving_declarations_are_required_at_start() {
        let mut config = config();
        config.cell.advertised_power.cpich_tx_dbm = None;
        assert!(validate(&config).is_err());
        config.cell.advertised_power.cpich_tx_dbm = Some(30);
        config.cell.advertised_power.uplink_interference_dbm = None;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn controller_wait_override_leaves_radio_wait_unchanged() {
        let mut config = config();
        assert_eq!(controller_idle_wait(&config), Duration::from_micros(100));
        config.runtime.idle_wait_us = 200;
        assert_eq!(controller_idle_wait(&config), Duration::from_micros(200));
        config.runtime.controller_idle_wait_us = Some(300);
        assert_eq!(controller_idle_wait(&config), Duration::from_micros(300));
        assert_eq!(
            epoch_settings(&config).unwrap().idle_wait,
            Duration::from_micros(200)
        );
    }

    #[test]
    fn serving_settings_parse_and_construct_epoch_settings() {
        let config = config();
        let settings = epoch_settings(&config).expect("serving settings are valid");
        assert_eq!(settings.idle_wait, Duration::from_micros(100));
        assert_eq!(settings.startup_timeout, Duration::from_millis(5000));
        assert_eq!(settings.clock_stall_timeout, Duration::from_millis(1000));
        assert_eq!(settings.control_origin, ControlTime(0));
        assert_eq!(settings.health.poll_interval, HEALTH_POLL_INTERVAL);
        assert_eq!(settings.health.maximum_underflows, MAXIMUM_HEALTH_EVENTS);
        assert_eq!(settings.health.maximum_late_packets, MAXIMUM_HEALTH_EVENTS);
        assert_eq!(
            settings.health.maximum_unknown_events,
            MAXIMUM_HEALTH_EVENTS
        );
    }

    #[test]
    fn serving_submission_deadlines_are_ordered() {
        let mut config = config();
        config.runtime.maximum_submission_us = config.runtime.minimum_submission_us - 1;
        assert!(validate(&config).is_err());
        config.runtime.maximum_submission_us = 1000;
        config.runtime.maximum_preparation_us = 999;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn serving_timing_budget_covers_a_slot_and_radio_delay() {
        let mut config = config();
        config.runtime.maximum_submission_us = 766;
        assert!(validate(&config).is_err());
        config.runtime.maximum_submission_us = 1000;
        config.runtime.maximum_preparation_us = 1525;
        validate(&config).expect("slot-end preparation lead leaves room for AICH response");
        // Check both supported profiles explicitly: changing the default filter
        // span changes the available AICH margin, not the timing rule.
        for (filter_span, last_valid_us, first_invalid_us) in [(16, 2195, 2196), (32, 2191, 2192)] {
            config.receiver.filter_span_chips = filter_span;
            config.runtime.maximum_preparation_us = last_valid_us;
            validate(&config).expect("rounded preparation budget leaves AICH margin");
            config.runtime.maximum_preparation_us = first_invalid_us;
            assert!(
                validate(&config)
                    .unwrap_err()
                    .to_string()
                    .contains("timing must remain below"),
                "filter_span={filter_span} preparation_us={first_invalid_us}"
            );
        }
    }

    #[test]
    fn aligned_two_slot_batch_reserves_aich_and_first_sample_lead() {
        let mut config = config();
        config.runtime.tx_batch_slots = 2;
        config.runtime.maximum_submission_us = 2334;
        config.runtime.maximum_preparation_us = 2434;
        validate(&config).unwrap();
        for (filter_span, last_valid_us, first_invalid_us) in [(16, 2862, 2863), (32, 2858, 2859)] {
            config.receiver.filter_span_chips = filter_span;
            config.runtime.maximum_preparation_us = last_valid_us;
            validate(&config).expect("aligned batch retains AICH margin");
            config.runtime.maximum_preparation_us = first_invalid_us;
            assert!(
                validate(&config)
                    .unwrap_err()
                    .to_string()
                    .contains("timing must remain below"),
                "filter_span={filter_span} preparation_us={first_invalid_us}"
            );
        }
        config.runtime.tx_batch_slots = 3;
        assert!(validate(&config).is_err());
    }
}
