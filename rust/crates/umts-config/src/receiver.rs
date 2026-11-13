// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrachReceiverSettings {
    pub minimum_coherent_fraction: f64,
}

impl Default for PrachReceiverSettings {
    fn default() -> Self {
        Self {
            minimum_coherent_fraction: 0.05,
        }
    }
}

impl PrachReceiverSettings {
    pub(crate) fn validate_schema(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.minimum_coherent_fraction.is_finite()
                && self.minimum_coherent_fraction > 0.0
                && self.minimum_coherent_fraction <= 1.0,
            "receiver.prach.minimum_coherent_fraction must be finite and in (0, 1]"
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
        assert_eq!(settings.minimum_coherent_fraction, 0.05);
        for value in [0.0, -0.1, 1.01, f64::NAN, f64::INFINITY] {
            assert!(
                PrachReceiverSettings {
                    minimum_coherent_fraction: value
                }
                .validate_schema()
                .is_err()
            );
        }
        for value in [0.01, 0.05, 1.0] {
            PrachReceiverSettings {
                minimum_coherent_fraction: value,
            }
            .validate_schema()
            .unwrap();
        }
        assert!(serde_yaml::from_str::<PrachReceiverSettings>("unknown: 1").is_err());
    }
}
