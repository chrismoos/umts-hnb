// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Result, bail};

use crate::{Config, SecondaryCarrierSettings};

const MAXIMUM_PRIMARY_SCRAMBLING_CODE: u16 = 511;
const DUAL_CELL_SAMPLES_PER_CHIP: u8 = 4;
const DUAL_CELL_OCCUPIED_BANDWIDTH_HZ: u64 = 10_000_000;
const TOTAL_POWER_PERCENT: f64 = 100.0;

impl SecondaryCarrierSettings {
    pub fn resolved_scrambling_code(&self, primary: u16) -> Result<u16> {
        let code = match self.primary_scrambling_code {
            Some(code) => code,
            None => primary.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!("secondary carrier default scrambling code overflows")
            })?,
        };
        if primary > MAXIMUM_PRIMARY_SCRAMBLING_CODE
            || code > MAXIMUM_PRIMARY_SCRAMBLING_CODE
            || code == primary
        {
            bail!(
                "cell.secondary_carrier.primary_scrambling_code must be in 0..=511 and differ from the primary carrier code"
            );
        }
        Ok(code)
    }
}

impl Config {
    pub(crate) fn validate_secondary_carrier(&self) -> Result<()> {
        let secondary = &self.cell.secondary_carrier;
        if self.hspa.downlink.dual_cell && (!self.hspa.downlink.enabled || !secondary.enabled) {
            bail!("hspa.downlink.dual_cell requires HSDPA and cell.secondary_carrier.enabled");
        }
        if !secondary.power_percent.is_finite()
            || secondary.power_percent <= 0.0
            || secondary.power_percent >= TOTAL_POWER_PERCENT
        {
            bail!(
                "cell.secondary_carrier.power_percent must be finite and greater than 0 and less than 100"
            );
        }
        if secondary.primary_scrambling_code.is_some() || secondary.enabled {
            secondary.resolved_scrambling_code(self.cell.physical.downlink_scrambling_code)?;
        }
        if !secondary.enabled {
            return Ok(());
        }
        if secondary.uarfcn_dl.is_none() {
            bail!(
                "cell.secondary_carrier.uarfcn_dl is required when the secondary carrier is enabled"
            );
        }
        if self.radio.tx_samples_per_chip != DUAL_CELL_SAMPLES_PER_CHIP {
            bail!("cell.secondary_carrier.enabled requires radio.tx_samples_per_chip: 4");
        }
        let required_bandwidth = self
            .radio
            .tx_lo_offset_hz
            .unsigned_abs()
            .checked_mul(2)
            .and_then(|offset| offset.checked_add(DUAL_CELL_OCCUPIED_BANDWIDTH_HZ));
        if required_bandwidth
            .is_none_or(|required| u64::from(self.radio.tx_analog_bandwidth_hz) < required)
        {
            bail!(
                "secondary carrier requires TX analog bandwidth of at least 10000000 Hz plus twice the absolute TX LO offset"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secondary_carrier_defaults_do_not_enable_transmission() {
        let secondary = SecondaryCarrierSettings::default();
        assert!(!secondary.enabled);
        assert_eq!(secondary.uarfcn_dl, None);
        assert_eq!(secondary.power_percent, 50.0);
        assert_eq!(secondary.resolved_scrambling_code(469).unwrap(), 470);
    }

    #[test]
    fn default_scrambling_code_does_not_wrap() {
        let mut secondary = SecondaryCarrierSettings::default();
        assert!(secondary.resolved_scrambling_code(511).is_err());
        assert!(secondary.resolved_scrambling_code(u16::MAX).is_err());
        secondary.primary_scrambling_code = Some(0);
        assert_eq!(secondary.resolved_scrambling_code(511).unwrap(), 0);
        assert!(secondary.resolved_scrambling_code(0).is_err());
        secondary.primary_scrambling_code = Some(512);
        assert!(secondary.resolved_scrambling_code(0).is_err());
    }
}
