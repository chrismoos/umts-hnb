// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrachReceiverSettings {
    pub minimum_peak_to_background_ratio: f64,
}

impl Default for PrachReceiverSettings {
    fn default() -> Self {
        Self {
            minimum_peak_to_background_ratio: 80.0,
        }
    }
}

impl PrachReceiverSettings {
    pub(crate) fn validate_schema(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.minimum_peak_to_background_ratio.is_finite()
                && self.minimum_peak_to_background_ratio > 1.0,
            "receiver.prach.minimum_peak_to_background_ratio must be finite and greater than 1"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_sensitivity_is_bounded_and_preserves_its_default() {
        let settings: PrachReceiverSettings = serde_yaml::from_str("{}").unwrap();
        assert_eq!(settings.minimum_peak_to_background_ratio, 80.0);
        for value in [0.0, -0.1, 1.0, f64::NAN, f64::INFINITY] {
            assert!(
                PrachReceiverSettings {
                    minimum_peak_to_background_ratio: value
                }
                .validate_schema()
                .is_err()
            );
        }
        for value in [2.0, 20.0, 1_000_000.0] {
            PrachReceiverSettings {
                minimum_peak_to_background_ratio: value,
            }
            .validate_schema()
            .unwrap();
        }
        assert!(serde_yaml::from_str::<PrachReceiverSettings>("unknown: 1").is_err());
        assert!(
            serde_yaml::from_str::<PrachReceiverSettings>("minimum_coherent_fraction: 0.05")
                .is_err()
        );
    }
}
