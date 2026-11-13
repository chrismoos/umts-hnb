// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::radio_resources::{HsdpaCodeLease, SecondaryRadioResources};
use std::num::NonZeroU8;
use umts_dsp::scrambling::PrimaryScramblingCode;

const SECONDARY_DATA_CODES: u8 = 15;
const MAXIMUM_COMBINED_CONTROL_CODES: u8 = 6;
const SECONDARY_CONTROL_CODES: u8 = 2;
const DEFAULT_MEASUREMENT_POWER_OFFSET_DB: f64 = 6.0;

use crate::controller::SecondaryHsdpaAdmission;

pub(super) struct SecondaryChannels {
    resources: SecondaryRadioResources,
    lease: Option<HsdpaCodeLease>,
    admission: SecondaryHsdpaAdmission,
    capacity: NonZeroUsize,
    maximum_bytes: NonZeroUsize,
}

impl SecondaryChannels {
    pub(super) fn reserve(
        config: &Config,
        epoch: RadioEpoch,
        cell: &CellInformation,
        power: Option<&super::super::downlink_power::Plan>,
    ) -> Result<Option<Self>> {
        if !config.hspa.downlink.dual_cell {
            return Ok(None);
        }
        ensure!(
            config.hspa.downlink.enabled && config.cell.secondary_carrier.enabled,
            "dual-cell HSDPA requires both downlink carriers enabled"
        );
        let settings = &config.hspa.downlink.resources;
        let scheduling = settings
            .scheduling
            .as_ref()
            .context("dual-cell HSDPA requires scheduler settings")?;
        ensure!(
            u16::from(settings.control_codes) + u16::from(SECONDARY_CONTROL_CODES)
                <= u16::from(MAXIMUM_COMBINED_CONTROL_CODES),
            "dual-cell HSDPA may configure at most six HS-SCCH codes across both cells"
        );
        let primary = PrimaryScramblingCode::new(
            config
                .cell
                .secondary_carrier
                .resolved_scrambling_code(cell.primary_code().index())?,
        )
        .map_err(|error| anyhow!("secondary scrambling code: {error:?}"))?;
        let mut resources = SecondaryRadioResources::new(epoch, cell, primary)
            .map_err(|error| anyhow!("secondary carrier resources: {error:?}"))?;
        let lease = resources
            .reserve_hsdpa(
                NonZeroU8::new(SECONDARY_DATA_CODES).unwrap(),
                NonZeroU8::new(SECONDARY_CONTROL_CODES).unwrap(),
            )
            .map_err(|error| anyhow!("reserve secondary HSDPA codes: {error:?}"))?;
        let capacity = NonZeroUsize::new(usize::from(settings.pending_transmissions))
            .context("secondary HSDPA pending capacity is zero")?;
        let required = crate::hsdpa_tx::TransmitBank::required_storage_bytes(capacity)
            .map_err(|error| anyhow!("secondary HSDPA storage: {error:?}"))?;
        ensure!(
            required
                .checked_mul(2)
                .is_some_and(|bytes| bytes <= settings.maximum_tx_bytes as usize),
            "hspa.downlink.resources.maximum_tx_bytes must cover both HSDPA transmit banks"
        );
        let gamma = power.and_then(|power| power.gamma_db);
        let admission = SecondaryHsdpaAdmission {
            primary,
            uarfcn_dl: u16::try_from(
                config
                    .cell
                    .secondary_carrier
                    .uarfcn_dl
                    .context("secondary downlink UARFCN missing")?,
            )
            .context("secondary downlink UARFCN exceeds the RRC field range")?,
            codes: *lease.codes(),
            measurement_power_offset_db: config
                .hspa
                .downlink
                .measurement_power_offset_db
                .or(gamma)
                .unwrap_or(DEFAULT_MEASUREMENT_POWER_OFFSET_DB),
            cpich_amplitude: power.map_or(config.power_control.downlink.cpich_amplitude, |power| {
                power.common.primary.cpich
            }),
            control_amplitude: power
                .and_then(|power| power.hs_scch_amplitude)
                .unwrap_or(scheduling.control_amplitude),
            power_limit: power.and_then(|power| power.hsdpa_power_limit),
            transmit_gamma_db: gamma,
            downlink_origin: None,
        };
        Ok(Some(Self {
            resources,
            lease: Some(lease),
            admission,
            capacity,
            maximum_bytes: NonZeroUsize::new(required).context("secondary TX storage is zero")?,
        }))
    }

    pub(super) fn admission(&self) -> SecondaryHsdpaAdmission {
        self.admission
    }

    pub(super) fn prepare_transmit(
        &mut self,
        source: &crate::common_tx::CommonTx,
    ) -> Result<crate::hsdpa_tx::TransmitBank> {
        let origin = ChipInstant {
            index: source
                .next_chip()
                .index
                .checked_add(crate::hsdpa_tx::CONTROL_LEAD_CHIPS)
                .context("secondary HSDPA origin overflow")?,
            ..source.next_chip()
        };
        let lease = self
            .lease
            .take()
            .context("secondary HSDPA TX owner already prepared")?;
        match crate::hsdpa_tx::TransmitBank::for_carrier(
            source,
            self.resources.scrambling_code(),
            lease,
            self.capacity,
            self.maximum_bytes,
        ) {
            Ok(bank) => {
                self.admission.downlink_origin = Some(origin);
                Ok(bank)
            }
            Err((error, lease)) => {
                self.lease = Some(lease);
                Err(anyhow!("secondary HSDPA TX owner: {error:?}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{CellPlan, ReferencePowers};
    use crate::test_support::at;

    #[test]
    fn disabled_secondary_does_not_reserve_a_bank() {
        let config: Config = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-support/fixtures/config/hsdpa.yaml"
        )))
        .unwrap();
        let cell = CellPlan::compile(
            &config,
            at(0).epoch,
            1,
            ReferencePowers {
                cpich_tx_dbm: 10,
                uplink_interference_dbm: -90,
            },
        )
        .unwrap();
        assert!(
            SecondaryChannels::reserve(&config, at(0).epoch, cell.information(), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn secondary_allocation_owns_full_data_tree_and_requires_both_bank_budgets() {
        let mut config: Config = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-support/fixtures/config/hsdpa.yaml"
        )))
        .unwrap();
        let cell = CellPlan::compile(
            &config,
            at(0).epoch,
            1,
            ReferencePowers {
                cpich_tx_dbm: 10,
                uplink_interference_dbm: -90,
            },
        )
        .unwrap();
        let defaults: serde_yaml::Value = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../config/defaults.yaml"
        )))
        .unwrap();
        config.hspa.downlink.resources.scheduling = Some(
            serde_yaml::from_value(defaults["hspa"]["downlink"]["resources"]["scheduling"].clone())
                .unwrap(),
        );
        config.hspa.downlink.enabled = true;
        config.hspa.downlink.dual_cell = true;
        config.cell.secondary_carrier.enabled = true;
        config.cell.secondary_carrier.uarfcn_dl = Some(config.radio.uarfcn_dl + 25);
        config.hspa.downlink.resources.control_codes = 4;
        let capacity = NonZeroUsize::new(usize::from(
            config.hspa.downlink.resources.pending_transmissions,
        ))
        .unwrap();
        let required = crate::hsdpa_tx::TransmitBank::required_storage_bytes(capacity).unwrap();
        config.hspa.downlink.resources.maximum_tx_bytes = u32::try_from(2 * required).unwrap();
        let secondary = SecondaryChannels::reserve(&config, at(0).epoch, cell.information(), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            secondary.admission.codes.data_count().get(),
            SECONDARY_DATA_CODES
        );
        assert_eq!(
            secondary.admission.codes.control_codes().count(),
            usize::from(SECONDARY_CONTROL_CODES)
        );
        assert_ne!(
            secondary.admission.primary,
            cell.information().primary_code()
        );
        config.hspa.downlink.resources.maximum_tx_bytes -= 1;
        assert!(
            SecondaryChannels::reserve(&config, at(0).epoch, cell.information(), None).is_err()
        );
    }
}
