// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    hsdpa_rx::{EvidenceLimits, FeedbackPolicy},
    hsdpa_session::{
        CellSchedulingBudget, DownlinkPower, SchedulingAdmission, SchedulingBudget,
        SchedulingSettings,
    },
};

pub(super) fn admission(
    config: &Config,
    budget: Option<&super::super::downlink_power::Plan>,
) -> Result<Option<SchedulingAdmission>> {
    let Some(settings) = &config.hspa.downlink.resources.scheduling else {
        return Ok(None);
    };
    settings.validate_schema()?;
    ensure!(
        settings.maximum_ue_bytes <= settings.maximum_cell_bytes,
        "HSDPA cell scheduler budget cannot admit one UE"
    );
    let count = |value: u32| -> Result<NonZeroUsize> {
        usize::try_from(value)
            .ok()
            .and_then(NonZeroUsize::new)
            .context("HSDPA scheduling budget must be a nonzero host-size value")
    };
    let evidence = |source: &umts_config::RustHsdpaEvidence| -> Result<EvidenceLimits> {
        EvidenceLimits::new(
            source.minimum_correlation,
            source.minimum_margin,
            source.minimum_pilot_fraction,
            source.maximum_relative_disturbance,
            source.minimum_relative_soft_power,
        )
        .map_err(|error| anyhow!("invalid HSDPA feedback evidence limits: {error:?}"))
    };
    let mut power = DownlinkPower::new(
        budget.map_or(config.power_control.downlink.cpich_amplitude, |power| {
            power.common.primary.cpich
        }),
        budget
            .and_then(|power| power.hs_scch_amplitude)
            .unwrap_or(settings.control_amplitude),
    )
    .map_err(|error| anyhow!("invalid HSDPA reference power: {error:?}"))?;
    if let Some((maximum, gamma)) =
        budget.and_then(|budget| budget.hsdpa_power_limit.zip(budget.gamma_db))
    {
        power = power
            .with_allocation(maximum, gamma as f32)
            .map_err(|error| anyhow!("invalid HSDPA power reservation: {error:?}"))?;
    }
    Ok(Some(SchedulingAdmission {
        settings: SchedulingSettings {
            power,
            maximum_cqi_age: NonZeroU64::new(u64::from(settings.maximum_cqi_age_chips))
                .context("HSDPA CQI age must be positive")?,
            selection_budget: count(settings.selection_budget)?,
            cqi_offset_limit: settings.cqi_offset_limit,
            cqi_target_bler_percent: settings.cqi_target_bler_percent,
        },
        budget: SchedulingBudget {
            maximum_formats: count(settings.maximum_formats)?,
            maximum_coded_bits: count(settings.maximum_coded_bits)?,
            maximum_plan_bytes: count(settings.maximum_plan_bytes)?,
            maximum_buffer_bytes: count(settings.maximum_buffer_bytes)?,
            maximum_encoder_bytes: count(settings.maximum_encoder_bytes)?,
            maximum_total_bytes: count(settings.maximum_ue_bytes)?,
        },
        feedback: FeedbackPolicy {
            acknowledgement: evidence(&settings.acknowledgement)?,
            cqi: evidence(&settings.cqi)?,
        },
        cell: CellSchedulingBudget::new(count(settings.maximum_cell_bytes)?),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_requires_explicit_feedback_and_checks_budget_before_admission() {
        let mut config = super::super::tests::config();
        assert!(admission(&config, None).unwrap().is_none());
        let source = r#"
maximum_formats: 1024
maximum_coded_bits: 100000
maximum_plan_bytes: 16000000
maximum_buffer_bytes: 2000000
maximum_encoder_bytes: 1000000
maximum_ue_bytes: 16000000
maximum_cell_bytes: 32000000
maximum_cqi_age_chips: 38400
selection_budget: 1024
control_amplitude: 0.1
acknowledgement: &evidence
  minimum_correlation: 0.5
  minimum_margin: 0.1
  minimum_pilot_fraction: 0.5
  maximum_relative_disturbance: 1.0
  minimum_relative_soft_power: 0.01
cqi: *evidence
"#;
        config.hspa.downlink.resources.scheduling = Some(serde_yaml::from_str(source).unwrap());
        let admitted = admission(&config, None).unwrap().unwrap();
        assert_eq!(admitted.cell.available_bytes(), 32_000_000);
        assert_eq!(admitted.budget.maximum_total_bytes.get(), 16_000_000);
        let settings = config.hspa.downlink.resources.scheduling.as_mut().unwrap();
        settings.maximum_cell_bytes = 1;
        assert!(admission(&config, None).is_err());
        let settings = config.hspa.downlink.resources.scheduling.as_mut().unwrap();
        settings.maximum_cell_bytes = 32_000_000;
        settings.cqi.minimum_relative_soft_power = f64::NAN;
        assert!(admission(&config, None).is_err());
        assert!(
            serde_yaml::from_str::<umts_config::RustHsdpaScheduling>(
                source.replace("cqi: *evidence", "").as_str()
            )
            .is_err()
        );
    }
}
