// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::Config;

const MODE_PATH: &str = "cell.packet_modes";
const SECONDARY_CHANNEL_PATH: &str = "cell.secondary_carrier.uarfcn_dl";
const DUAL_TX_SAMPLES_PER_CHIP: u8 = 4;
const SINGLE_TX_SAMPLES_PER_CHIP: u8 = 2;
const DUAL_OCCUPIED_BANDWIDTH_HZ: u64 = 10_000_000;
const SINGLE_OCCUPIED_BANDWIDTH_HZ: u64 = 5_000_000;
/// Moves LO leakage off a single carrier. Between two carriers the LO already sits in the gap.
const SINGLE_TX_LO_OFFSET_HZ: i64 = 2_500_000;
const TX_LO_OFFSET_PATH: &str = "radio.tx_lo_offset_hz";
const TX_LO_OFFSET_BACKEND: &str = "uhd";
const REMOVED_MODE_SWITCHES: [&str; 5] = [
    "hspa.downlink.enabled",
    "hspa.uplink.enabled",
    "hspa.downlink.qam64_enabled",
    "hspa.downlink.dual_cell",
    "cell.secondary_carrier.enabled",
];

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PacketMode {
    Rel99,
    Hspa,
    HspaPlus,
    DcHspa,
}

impl Config {
    pub(crate) fn validate_packet_modes(&self) -> Result<()> {
        self.validate_packet_mode_list()?;
        if let Some(modes) = &self.cell.packet_modes {
            let high_speed = modes.iter().any(|mode| *mode != PacketMode::Rel99);
            let dual = modes.contains(&PacketMode::DcHspa);
            let qam64 = dual || modes.contains(&PacketMode::HspaPlus);
            if self.hspa.downlink.enabled != high_speed
                || self.hspa.uplink.enabled != high_speed
                || self.hspa.downlink.qam64_enabled != qam64
                || self.hspa.downlink.dual_cell != dual
                || self.cell.secondary_carrier.enabled != dual
            {
                bail!(
                    "cell.packet_modes requires resolved feature switches, load configuration through Config::load or Deployment"
                );
            }
        }
        Ok(())
    }

    fn validate_packet_mode_list(&self) -> Result<()> {
        if let Some(modes) = &self.cell.packet_modes {
            if modes.is_empty() {
                bail!("cell.packet_modes must contain at least one mode");
            }
            for (index, mode) in modes.iter().enumerate() {
                if modes[..index].contains(mode) {
                    bail!("cell.packet_modes must not contain duplicate modes");
                }
            }
        }
        Ok(())
    }

    pub(crate) fn resolve_packet_modes(&mut self, explicit: &Value) -> Result<()> {
        let specified_modes = lookup(explicit, MODE_PATH);
        if specified_modes.is_some_and(Value::is_null) {
            bail!("cell.packet_modes must be a nonempty list, not null");
        }
        for path in REMOVED_MODE_SWITCHES {
            if lookup(explicit, path).is_some() {
                bail!("{path} cannot be set directly, select packet features with {MODE_PATH}");
            }
        }
        self.validate_packet_mode_list()?;
        let Some(modes) = &self.cell.packet_modes else {
            return Ok(());
        };
        let high_speed = modes.iter().any(|mode| *mode != PacketMode::Rel99);
        let dual = modes.contains(&PacketMode::DcHspa);
        let qam64 = dual || modes.contains(&PacketMode::HspaPlus);
        if !high_speed {
            for path in [
                "hspa.downlink.compact_common_channels",
                "hspa.downlink.fdpch",
                "hspa.uplink.four_code_uplink",
            ] {
                check_switch(explicit, path, false)?;
            }
        }
        if dual {
            let channel = lookup(explicit, SECONDARY_CHANNEL_PATH)
                .filter(|value| !value.is_null())
                .context("dc-hspa requires an explicit cell.secondary_carrier.uarfcn_dl in the operator configuration")?;
            let channel: u32 = serde_yaml::from_value(channel.clone())
                .context("invalid explicit secondary carrier UARFCN")?;
            if self.cell.secondary_carrier.uarfcn_dl != Some(channel) {
                bail!("resolved secondary carrier differs from the explicit operator channel");
            }
            if let Some(value) = lookup(explicit, "radio.tx_samples_per_chip")
                && serde_yaml::from_value::<u8>(value.clone())? != DUAL_TX_SAMPLES_PER_CHIP
            {
                bail!("dc-hspa requires radio.tx_samples_per_chip: 4");
            }
            self.radio.tx_samples_per_chip = DUAL_TX_SAMPLES_PER_CHIP;
        } else if lookup(explicit, "radio.tx_samples_per_chip").is_none() {
            self.radio.tx_samples_per_chip = SINGLE_TX_SAMPLES_PER_CHIP;
        }
        if lookup(explicit, TX_LO_OFFSET_PATH).is_none() {
            self.radio.tx_lo_offset_hz = if dual || self.radio.backend != TX_LO_OFFSET_BACKEND {
                0
            } else {
                SINGLE_TX_LO_OFFSET_HZ
            };
        }
        let occupied = if dual {
            DUAL_OCCUPIED_BANDWIDTH_HZ
        } else {
            SINGLE_OCCUPIED_BANDWIDTH_HZ
        };
        let required_bandwidth = self
            .radio
            .tx_lo_offset_hz
            .unsigned_abs()
            .checked_mul(2)
            .and_then(|offset| offset.checked_add(occupied))
            .and_then(|value| u32::try_from(value).ok())
            .context("cell.packet_modes required TX bandwidth exceeds the supported range")?;
        if lookup(explicit, "radio.tx_analog_bandwidth_hz").is_some() {
            if self.radio.tx_analog_bandwidth_hz < required_bandwidth {
                bail!(
                    "cell.packet_modes requires radio.tx_analog_bandwidth_hz of at least {required_bandwidth} Hz for the configured LO offset"
                );
            }
        } else {
            self.radio.tx_analog_bandwidth_hz =
                self.radio.tx_analog_bandwidth_hz.max(required_bandwidth);
        }
        self.hspa.downlink.enabled = high_speed;
        self.hspa.uplink.enabled = high_speed;
        self.hspa.downlink.qam64_enabled = qam64;
        self.hspa.downlink.dual_cell = dual;
        self.cell.secondary_carrier.enabled = dual;
        if !high_speed {
            self.hspa.downlink.compact_common_channels = false;
            self.hspa.downlink.fdpch = false;
            self.hspa.uplink.four_code_uplink = false;
        } else if lookup(explicit, "hspa.uplink.four_code_uplink").is_none() {
            self.hspa.uplink.four_code_uplink &=
                self.hspa.uplink.scheduled && self.hspa.uplink.max_category >= 6;
        }
        Ok(())
    }
}

fn lookup<'a>(mut value: &'a Value, path: &str) -> Option<&'a Value> {
    for key in path.split('.') {
        value = value.as_mapping()?.get(Value::String(key.to_owned()))?;
    }
    Some(value)
}

fn check_switch(explicit: &Value, path: &str, expected: bool) -> Result<()> {
    if let Some(value) = lookup(explicit, path)
        && serde_yaml::from_value::<bool>(value.clone())? != expected
    {
        bail!(
            "{path} conflicts with cell.packet_modes, remove the old switch or set it to {expected}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Deployment;

    const DEFAULTS: &str = include_str!("../../../../config/defaults.yaml");
    const OPERATOR: &str = include_str!("../../../test-support/fixtures/config/operator.yaml");

    fn operator(modes: Option<&[PacketMode]>) -> Value {
        let mut value: Value = serde_yaml::from_str(OPERATOR).unwrap();
        value.as_mapping_mut().unwrap().remove("hspa");
        if let Some(modes) = modes {
            value["cell"]["packet_modes"] = serde_yaml::to_value(modes).unwrap();
        }
        value
    }

    fn parse(value: &Value) -> Result<Deployment> {
        Deployment::parse(DEFAULTS, &serde_yaml::to_string(value).unwrap())
    }

    fn dc_operator() -> Value {
        let mut value = operator(Some(&[PacketMode::DcHspa]));
        value["cell"]["secondary_carrier"] = serde_yaml::from_str("uarfcn_dl: 4425").unwrap();
        value
    }

    #[test]
    fn shipped_default_resolves_all_single_carrier_modes_and_no_secondary_transmitter() {
        let loaded = parse(&operator(None)).unwrap();
        assert_eq!(
            loaded.config.cell.packet_modes.as_deref(),
            Some([PacketMode::Rel99, PacketMode::Hspa, PacketMode::HspaPlus].as_slice())
        );
        assert!(loaded.config.hspa.downlink.enabled);
        assert!(loaded.config.hspa.uplink.enabled);
        assert!(loaded.config.hspa.downlink.qam64_enabled);
        assert!(!loaded.config.hspa.downlink.dual_cell);
        assert!(!loaded.config.cell.secondary_carrier.enabled);
        assert_eq!(loaded.config.radio.tx_samples_per_chip, 2);
        assert_eq!(
            loaded.effective["cell"]["packet_modes"],
            serde_yaml::to_value([PacketMode::Rel99, PacketMode::Hspa, PacketMode::HspaPlus])
                .unwrap()
        );
    }

    #[test]
    fn every_mode_subset_resolves_its_feature_union_and_required_dc_geometry() {
        let all = [
            PacketMode::Rel99,
            PacketMode::Hspa,
            PacketMode::HspaPlus,
            PacketMode::DcHspa,
        ];
        for mask in 1..(1 << all.len()) {
            let modes: Vec<_> = all
                .iter()
                .enumerate()
                .filter_map(|(i, mode)| (mask & (1 << i) != 0).then_some(*mode))
                .collect();
            let mut value = operator(Some(&modes));
            let dual = modes.contains(&PacketMode::DcHspa);
            if dual {
                value["cell"]["secondary_carrier"] =
                    serde_yaml::from_str("uarfcn_dl: 4425").unwrap();
            }
            let config = parse(&value).unwrap().config;
            let high_speed = modes.iter().any(|mode| *mode != PacketMode::Rel99);
            assert_eq!(config.hspa.downlink.enabled, high_speed);
            assert_eq!(config.hspa.uplink.enabled, high_speed);
            assert_eq!(
                config.hspa.downlink.qam64_enabled,
                dual || modes.contains(&PacketMode::HspaPlus)
            );
            assert_eq!(config.hspa.downlink.dual_cell, dual);
            assert_eq!(config.cell.secondary_carrier.enabled, dual);
            assert_eq!(config.radio.tx_samples_per_chip, if dual { 4 } else { 2 });
            if dual {
                assert!(config.radio.tx_analog_bandwidth_hz >= 10_000_000);
                assert_eq!(
                    config
                        .cell
                        .secondary_carrier
                        .resolved_scrambling_code(0)
                        .unwrap(),
                    1
                );
                assert_eq!(config.cell.secondary_carrier.power_percent, 50.0);
            }
            if !high_speed {
                assert!(!config.hspa.downlink.compact_common_channels);
                assert!(!config.hspa.downlink.fdpch);
                assert!(!config.hspa.uplink.four_code_uplink);
            }
        }
    }

    #[test]
    fn invalid_mode_lists_and_null_are_rejected() {
        for list in [
            "[]",
            "null",
            "[hspa, hspa]",
            "[HSPA]",
            "[unknown]",
            "[1]",
            "hspa",
        ] {
            let mut value = operator(None);
            value["cell"]["packet_modes"] = serde_yaml::from_str(list).unwrap();
            assert!(parse(&value).is_err(), "accepted {list}");
        }
    }

    #[test]
    fn dc_requires_an_operator_channel_even_if_system_defaults_supply_one() {
        let value = operator(Some(&[PacketMode::DcHspa]));
        assert!(
            parse(&value)
                .err()
                .unwrap()
                .to_string()
                .contains("explicit cell.secondary_carrier.uarfcn_dl")
        );
        let mut defaults: Value = serde_yaml::from_str(DEFAULTS).unwrap();
        defaults["cell"]["secondary_carrier"]["uarfcn_dl"] = Value::from(4425);
        assert!(
            Deployment::parse(
                &serde_yaml::to_string(&defaults).unwrap(),
                &serde_yaml::to_string(&value).unwrap()
            )
            .is_err()
        );
        let mut value = dc_operator();
        value["cell"]["secondary_carrier"]["uarfcn_dl"] = Value::Null;
        assert!(parse(&value).is_err());
    }

    #[test]
    fn removed_mode_switches_are_rejected_with_the_packet_modes_hint() {
        for path in REMOVED_MODE_SWITCHES {
            for item in [false, true] {
                let mut value = dc_operator();
                set(&mut value, path, Value::Bool(item));
                let error = parse(&value).err().unwrap().to_string();
                assert!(error.contains(path), "{error}");
                assert!(error.contains("cell.packet_modes"), "{error}");
                let mut value = operator(None);
                set(&mut value, path, Value::Bool(item));
                assert!(parse(&value).is_err(), "{path}");
            }
        }
    }

    #[test]
    fn standalone_load_rejects_removed_mode_switches() {
        let path =
            std::env::temp_dir().join(format!("umts-removed-switch-{}.yaml", std::process::id()));
        let mut value = operator(None);
        set(&mut value, "hspa.downlink.enabled", Value::Bool(true));
        std::fs::write(&path, serde_yaml::to_string(&value).unwrap()).unwrap();
        let error = Config::load(&path).err().unwrap().to_string();
        std::fs::remove_file(path).unwrap();
        assert!(error.contains("hspa.downlink.enabled") && error.contains("cell.packet_modes"));
    }

    #[test]
    fn explicit_conflicting_dependent_switches_are_rejected() {
        for path in [
            "hspa.downlink.compact_common_channels",
            "hspa.downlink.fdpch",
            "hspa.uplink.four_code_uplink",
        ] {
            let mut value = operator(Some(&[PacketMode::Rel99]));
            set(&mut value, path, Value::Bool(true));
            assert!(parse(&value).is_err());
        }
    }

    #[test]
    fn dc_derives_bandwidth_but_preserves_explicit_valid_geometry_and_radio_settings() {
        for offset in [-2_500_000_i64, 0, 2_500_000] {
            let mut value = dc_operator();
            value["radio"]["tx_lo_offset_hz"] = Value::from(offset);
            let loaded = parse(&value).unwrap();
            let required = 10_000_000 + offset.unsigned_abs() * 2;
            assert_eq!(
                u64::from(loaded.config.radio.tx_analog_bandwidth_hz),
                required
            );
            value["radio"]["tx_analog_bandwidth_hz"] = Value::from(required - 1);
            assert!(parse(&value).is_err());
            value["radio"]["tx_analog_bandwidth_hz"] = Value::from(required + 1);
            let config = parse(&value).unwrap().config;
            assert_eq!(u64::from(config.radio.tx_analog_bandwidth_hz), required + 1);
            assert_eq!(config.radio.tx_gain_db, 75.0);
            assert_eq!(config.radio.rx_gain_db, 30.0);
            assert_eq!(config.radio.tx_lo_offset_hz, offset);
            assert_eq!(config.radio.rx_delay_samples, 95.0);
            assert_eq!(
                config.runtime.maximum_preparation_us,
                loaded.config.runtime.maximum_preparation_us
            );
            assert_eq!(
                config.runtime.maximum_submission_us,
                loaded.config.runtime.maximum_submission_us
            );
        }
    }

    #[test]
    fn dc_rejects_explicit_narrow_samples_and_overflowing_bandwidth() {
        let mut value = dc_operator();
        value["radio"]["tx_samples_per_chip"] = Value::from(2);
        assert!(parse(&value).is_err());
        value["radio"]
            .as_mapping_mut()
            .unwrap()
            .remove("tx_samples_per_chip");
        for offset in [i64::MIN, i64::MAX, i64::from(u32::MAX)] {
            value["radio"]["tx_lo_offset_hz"] = Value::from(offset);
            assert!(parse(&value).is_err());
        }
    }

    #[test]
    fn non_dc_modes_preserve_explicit_wide_tx_geometry() {
        let mut value = operator(Some(&[PacketMode::HspaPlus]));
        value["radio"]["tx_samples_per_chip"] = Value::from(4);
        value["radio"]["tx_analog_bandwidth_hz"] = Value::from(15_360_000);
        let config = parse(&value).unwrap().config;
        assert_eq!(config.radio.tx_samples_per_chip, 4);
        assert_eq!(config.radio.tx_analog_bandwidth_hz, 15_360_000);
    }

    #[test]
    fn inherited_four_code_option_is_disabled_when_advanced_uplink_settings_cannot_support_it() {
        for (path, item) in [
            ("hspa.uplink.max_category", Value::from(5)),
            ("hspa.uplink.scheduled", Value::Bool(false)),
        ] {
            let mut value = operator(Some(&[PacketMode::HspaPlus]));
            set(&mut value, path, item);
            assert!(!parse(&value).unwrap().config.hspa.uplink.four_code_uplink);
            set(
                &mut value,
                "hspa.uplink.four_code_uplink",
                Value::Bool(true),
            );
            assert!(parse(&value).is_err());
        }
    }

    #[test]
    fn standalone_load_resolves_explicit_modes_and_rejects_null() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "umts-packet-modes-{}-{nonce}.yaml",
            std::process::id()
        ));
        let mut value = operator(Some(&[PacketMode::HspaPlus]));
        std::fs::write(&path, serde_yaml::to_string(&value).unwrap()).unwrap();
        let config = Config::load(&path).unwrap();
        assert!(
            config.hspa.downlink.enabled
                && config.hspa.uplink.enabled
                && config.hspa.downlink.qam64_enabled
        );
        assert_eq!(config.cell.packet_modes, Some(vec![PacketMode::HspaPlus]));
        value["cell"]["packet_modes"] = Value::Null;
        std::fs::write(&path, serde_yaml::to_string(&value).unwrap()).unwrap();
        assert!(Config::load(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unset_lo_offset_follows_the_carrier_count_and_backend() {
        let single = parse(&operator(Some(&[PacketMode::HspaPlus])))
            .unwrap()
            .config;
        assert_eq!(single.radio.tx_lo_offset_hz, SINGLE_TX_LO_OFFSET_HZ);
        assert_eq!(single.radio.tx_analog_bandwidth_hz, 10_000_000);

        let mut value = operator(Some(&[PacketMode::DcHspa]));
        value["cell"]["secondary_carrier"]["uarfcn_dl"] = Value::from(4425);
        let dual = parse(&value).unwrap().config;
        assert_eq!(dual.radio.tx_lo_offset_hz, 0);
        assert_eq!(dual.radio.tx_analog_bandwidth_hz, 10_000_000);

        let mut value = operator(Some(&[PacketMode::HspaPlus]));
        value["radio"]["backend"] = Value::from("bladerf");
        assert_eq!(parse(&value).unwrap().config.radio.tx_lo_offset_hz, 0);

        let mut value = operator(Some(&[PacketMode::HspaPlus]));
        value["radio"]["tx_lo_offset_hz"] = Value::from(0);
        let explicit = parse(&value).unwrap().config;
        assert_eq!(explicit.radio.tx_lo_offset_hz, 0);
        assert_eq!(explicit.radio.tx_analog_bandwidth_hz, 5_000_000);
    }

    #[test]
    fn invalid_system_mode_lists_are_rejected() {
        for list in ["[]", "[hspa, hspa]", "null", "[unknown]"] {
            let mut defaults: Value = serde_yaml::from_str(DEFAULTS).unwrap();
            defaults["cell"]["packet_modes"] = serde_yaml::from_str(list).unwrap();
            assert!(
                Deployment::parse(&serde_yaml::to_string(&defaults).unwrap(), OPERATOR).is_err()
            );
        }
    }

    #[test]
    fn directly_deserialized_unresolved_modes_fail_validation() {
        let value = operator(Some(&[PacketMode::HspaPlus]));
        let config: Config = serde_yaml::from_value(value).unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("resolved feature switches")
        );
    }

    #[test]
    fn single_carrier_modes_resolve_lo_bandwidth_without_changing_gain_or_timing() {
        let mut value = operator(Some(&[PacketMode::HspaPlus]));
        value["radio"]["tx_lo_offset_hz"] = Value::from(2_500_000);
        let loaded = parse(&value).unwrap();
        assert_eq!(loaded.config.radio.tx_analog_bandwidth_hz, 10_000_000);
        assert_eq!(loaded.config.radio.rx_gain_db, 30.0);
        assert_eq!(loaded.config.radio.rx_delay_samples, 95.0);
        value["radio"]["tx_analog_bandwidth_hz"] = Value::from(9_999_999);
        assert!(parse(&value).is_err());
    }

    #[test]
    fn dc_scrambling_code_boundary_requires_an_explicit_distinct_code() {
        let mut value = dc_operator();
        value["cell"]["physical"]["downlink_scrambling_code"] = Value::from(511);
        assert!(parse(&value).is_err());
        value["cell"]["secondary_carrier"]["primary_scrambling_code"] = Value::from(0);
        assert_eq!(
            parse(&value)
                .unwrap()
                .config
                .cell
                .secondary_carrier
                .resolved_scrambling_code(511)
                .unwrap(),
            0
        );
    }

    fn set(value: &mut Value, path: &str, item: Value) {
        if let Some((head, tail)) = path.split_once('.') {
            let child = value
                .as_mapping_mut()
                .unwrap()
                .entry(Value::String(head.to_owned()))
                .or_insert_with(|| Value::Mapping(serde_yaml::Mapping::new()));
            set(child, tail, item);
        } else {
            value
                .as_mapping_mut()
                .unwrap()
                .insert(Value::String(path.to_owned()), item);
        }
    }
}
