// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, ensure};
use umts_config::Config;
use umts_dsp::{common_downlink::CommonChannelAmplitudes, pulse_shaping::maximum_component_gain};

use crate::common_tx::CommonAmplitudes;

const PERCENT: f64 = 100.0;
const BCH_DUTY: f64 = 0.9;
const SCH_DUTY: f64 = 0.1;
const MAXIMUM_SYMBOL_MAGNITUDE: f64 = 2.0;
const MAXIMUM_CPICH_AMPLITUDE: f64 = 0.25;
const LAST_NORMAL_DPCH_FORMAT: u8 = 16;

fn maximum_dpch_average_multiplier(tpc_power: f64) -> f64 {
    // TS 25.211 Table 11 time-multiplexes TPC with the other DPCH fields.
    (0..=LAST_NORMAL_DPCH_FORMAT)
        .flat_map(|number| [false, true].map(move |tfci| (number, tfci)))
        .filter_map(|(number, tfci)| umts_dsp::dedicated::DownlinkFormat::new(number, tfci).ok())
        .map(|format| {
            let total = 2.0 * umts_types::time::CHIPS_PER_SLOT as f64
                / f64::from(format.spreading_factor());
            1.0 + format.tpc_bits() as f64 / total * (tpc_power - 1.0)
        })
        .fold(1.0, f64::max)
}

pub(super) fn hsdpa_transmission_slots(config: &Config) -> u32 {
    config
        .runtime
        .maximum_ues
        .min(u32::from(config.hspa.downlink.resources.control_codes))
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Plan {
    pub common: CommonAmplitudes,
    pub output_gain: f32,
    pub peak_component_bound: f32,
    pub component_peak_limit: f32,
    pub dynamic_power_limit: f32,
    pub packet_power_gain: f32,
    pub dedicated_offset_db: f32,
    pub dedicated_minimum_offset_db: f32,
    pub hs_scch_amplitude: Option<f32>,
    pub e_agch_amplitude: Option<f32>,
    pub e_hich_amplitude: Option<f32>,
    pub dedicated_users: u32,
    pub dedicated_codes_per_user: u8,
    pub hsdpa_power_limit: Option<f32>,
    pub gamma_db: Option<f64>,
    pub aich_offset_db: i8,
    pub pich_offset_db: i8,
}

fn indicator_offset(reserved: f64, cpich: f64, minimum: i8) -> Result<i8> {
    let db = (10.0 * (reserved / cpich).log10() + f64::EPSILON * 16.0).floor();
    ensure!(
        db >= f64::from(minimum),
        "downlink indicator allocation is below the minimum signallable power offset"
    );
    Ok(db.min(5.0) as i8)
}

impl Plan {
    pub(super) fn report(&self) {
        tracing::info!(
            "{}",
            serde_json::json!({
                "downlink_power_budget": {
                    "fixed_carrier_reference": true,
                    "output_gain": self.output_gain,
                    "peak_component_bound": self.peak_component_bound,
                    "component_peak_limit": self.component_peak_limit,
                    "dynamic_power_limit": self.dynamic_power_limit,
                    "dynamic_power_channels": "DPCH after HS-PDSCH power reservation",
                    "hsdpa_subframe_power_constant": true,
                    "peak_enforcement": "final_waveform_before_submission",
                    "dedicated_users": self.dedicated_users,
                    "dedicated_codes_per_user": self.dedicated_codes_per_user,
                    "cpich_rms": self.common.primary.cpich,
                    "pccpch_rms": self.common.primary.pccpch,
                    "primary_sch_rms": self.common.primary.primary_sch,
                    "secondary_sch_rms": self.common.primary.secondary_sch,
                    "fach_rms": self.common.fach,
                    "pch_rms": self.common.pch,
                    "hsdpa_power_limit": self.hsdpa_power_limit,
                    "gamma_db": self.gamma_db,
                    "aich_offset_db": self.aich_offset_db,
                    "pich_offset_db": self.pich_offset_db
                }
            })
        );
    }

    pub(super) fn resolve(config: &Config) -> Result<Option<Self>> {
        let Some(settings) = &config.power_control.downlink.allocation else {
            return Ok(None);
        };
        settings.validate_schema()?;
        let fractions = [
            settings.cpich_percent,
            settings.broadcast_percent,
            settings.fach_percent,
            settings.pch_percent,
            settings.pich_percent,
            settings.aich_percent,
            settings.dedicated_percent,
            settings.hsdpa_percent,
            settings.hs_scch_percent,
            settings.e_agch_percent,
            settings.e_hich_percent,
        ];
        let sum = fractions.iter().map(|&value| f64::from(value)).sum::<f64>();
        ensure!(
            sum <= PERCENT + 0.00001,
            "downlink power allocations exceed 100 percent"
        );
        let fraction = |value: f32| f64::from(value) / PERCENT;
        let cpich = fraction(settings.cpich_percent);
        let broadcast = fraction(settings.broadcast_percent);
        let fach = fraction(settings.fach_percent);
        let pch = fraction(settings.pch_percent);
        let pich = fraction(settings.pich_percent);
        let aich = fraction(settings.aich_percent);
        let dedicated = fraction(settings.dedicated_percent);
        let synchronization = fraction(settings.broadcast_sync_percent);
        let users = f64::from(config.runtime.maximum_ues);
        ensure!(
            users > 0.0,
            "downlink power budget requires a positive UE capacity"
        );
        let candidates = &config.rel99.physical.planner.physical_candidates;
        let codes = candidates
            .iter()
            .map(|candidate| candidate.downlink_code_count)
            .max()
            .context("downlink power budget requires dedicated physical candidates")?;
        ensure!(
            candidates
                .iter()
                .all(|candidate| candidate.pilot_offset_quarter_db == 0),
            "central downlink budget currently requires zero dedicated pilot offsets"
        );
        let codes_per_user = f64::from(codes);
        let primary = CommonChannelAmplitudes {
            cpich: cpich.sqrt() as f32,
            pccpch: (broadcast * (1.0 - synchronization) / BCH_DUTY).sqrt() as f32,
            primary_sch: (broadcast * synchronization / (2.0 * SCH_DUTY)).sqrt() as f32,
            secondary_sch: (broadcast * synchronization / (2.0 * SCH_DUTY)).sqrt() as f32,
        };
        let mut common = CommonAmplitudes {
            primary,
            fach: fach.sqrt() as f32,
            pch: pch.sqrt() as f32,
        };
        let aich_offset_db = indicator_offset(aich, cpich, -22)?;
        let pich_offset_db = indicator_offset(pich, cpich, -10)?;
        let indicator = |offset| cpich.sqrt() * 10.0_f64.powf(f64::from(offset) / 20.0);
        let dedicated_peak = (dedicated / (users * codes_per_user)).sqrt();
        let tpc_boost =
            10.0_f64.powf(f64::from(config.power_control.downlink.dedicated.tpc_offset_db) / 20.0);
        let dedicated_data =
            dedicated_peak / maximum_dpch_average_multiplier(tpc_boost * tpc_boost).sqrt();
        let dedicated_offset = 20.0 * (dedicated_data / cpich.sqrt()).log10();
        ensure!(
            (-20.0..=6.0).contains(&dedicated_offset),
            "dedicated allocation cannot cover the configured UE/code capacity within supported power offsets"
        );
        let mut chip_bound = f64::from(primary.cpich)
            + f64::from(primary.pccpch)
                .max(f64::from(primary.primary_sch) + f64::from(primary.secondary_sch))
            + f64::from(common.fach)
            + f64::from(common.pch)
            + indicator(aich_offset_db)
            + indicator(pich_offset_db)
            + users * codes_per_user * dedicated_data * tpc_boost.max(1.0);
        let mut gamma_db = None;
        let mut hsdpa_power_limit = None;
        let mut scch_amplitude = None;
        if config.hspa.downlink.enabled {
            let transmissions = f64::from(hsdpa_transmission_slots(config));
            ensure!(transmissions > 0.0, "HSDPA requires transmission capacity");
            let data_power = fraction(settings.hsdpa_percent) / transmissions;
            let control_codes = f64::from(config.hspa.downlink.resources.control_codes);
            let data_codes = f64::from(config.hspa.downlink.resources.data_codes);
            ensure!(
                data_power > 0.0 && control_codes > 0.0 && data_codes > 0.0,
                "HSDPA requires positive power and code reservations"
            );
            let gamma = (20.0 * (data_power / cpich).log10()).floor() * 0.5;
            ensure!(
                (-6.0..=13.0).contains(&gamma),
                "HSDPA allocation cannot represent a CQI measurement power offset for this UE capacity"
            );
            let control_power = fraction(settings.hs_scch_percent) / control_codes;
            ensure!(control_power > 0.0, "HSDPA requires positive HS-SCCH power");
            let control = control_power.sqrt();
            chip_bound += (data_codes * fraction(settings.hsdpa_percent)).sqrt()
                * MAXIMUM_SYMBOL_MAGNITUDE
                + control_codes * control;
            gamma_db = Some(gamma);
            hsdpa_power_limit = Some(data_power as f32);
            scch_amplitude = Some(control);
        }
        let mut grant_amplitude = None;
        let mut feedback_amplitude = None;
        if config.hspa.uplink.enabled {
            let grant = fraction(settings.e_agch_percent).sqrt();
            let feedback = (fraction(settings.e_hich_percent) / users).sqrt();
            ensure!(
                grant > 0.0 && feedback > 0.0,
                "HSUPA requires positive downlink control power"
            );
            // All users share one E-AGCH code, and its grants cannot overlap.
            chip_bound += grant + users * feedback;
            grant_amplitude = Some(grant);
            feedback_amplitude = Some(feedback);
        }
        // An L1 phase bound covers arbitrary code/data alignment and pulse-filter tails.
        let filter_bound = maximum_component_gain(
            super::radio::tx_samples_per_chip(config)?,
            usize::from(config.receiver.filter_span_chips),
        )
        .map_err(|error| anyhow::anyhow!("downlink pulse bound: {error:?}"))?;
        let output_limit = 10.0_f64.powf(-f64::from(settings.peak_margin_db) / 20.0);
        let rounded_limit = output_limit as f32;
        // Round below the configured peak margin rather than exceeding it during f32 conversion.
        let component_peak_limit = if f64::from(rounded_limit) >= output_limit {
            rounded_limit.next_down()
        } else {
            rounded_limit
        };
        let gain = f64::from(settings.carrier_rms);
        ensure!(
            gain.is_finite() && gain > 0.0,
            "downlink output scaling is invalid"
        );
        ensure!(
            cpich.sqrt() * gain <= MAXIMUM_CPICH_AMPLITUDE,
            "fixed carrier_rms={} exceeds the supported CPICH amplitude limit. Choose an explicit lower carrier reference, not automatic channel rescaling",
            settings.carrier_rms
        );
        let component_bound = chip_bound * filter_bound * gain;
        common.primary.cpich *= gain as f32;
        common.primary.pccpch *= gain as f32;
        common.primary.primary_sch *= gain as f32;
        common.primary.secondary_sch *= gain as f32;
        common.fach *= gain as f32;
        common.pch *= gain as f32;
        let dynamic_range = config.power_control.downlink.dedicated.downlink_offset_db
            - config
                .power_control
                .downlink
                .dedicated
                .downlink_minimum_offset_db;

        let plan = Self {
            common,
            output_gain: gain as f32,
            peak_component_bound: component_bound as f32,
            component_peak_limit,
            dynamic_power_limit: ((dedicated + fraction(settings.hsdpa_percent)) * gain * gain)
                as f32,
            packet_power_gain: ((settings.dedicated_percent + settings.hsdpa_percent)
                / settings.dedicated_percent)
                .sqrt(),
            dedicated_offset_db: dedicated_offset as f32,
            dedicated_minimum_offset_db: dedicated_offset as f32 - dynamic_range,
            hs_scch_amplitude: scch_amplitude.map(|value| (value * gain) as f32),
            e_agch_amplitude: grant_amplitude.map(|value| (value * gain) as f32),
            e_hich_amplitude: feedback_amplitude.map(|value| (value * gain) as f32),
            dedicated_users: config.runtime.maximum_ues,
            dedicated_codes_per_user: codes,
            hsdpa_power_limit: hsdpa_power_limit.map(|_| {
                common.primary.cpich.powi(2) * settings.hsdpa_percent
                    / settings.cpich_percent
                    / hsdpa_transmission_slots(config) as f32
            }),
            gamma_db,
            aich_offset_db,
            pich_offset_db,
        };
        Ok(Some(plan))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umts_config::RustDownlinkPowerConfig;
    use umts_dsp::pulse_shaping::SamplesPerChip;

    fn profile() -> Config {
        let mut config = super::super::tests::config();
        config.runtime.maximum_ues = 2;
        config.power_control.downlink.dedicated.downlink_offset_db = 0.0;
        config
            .power_control
            .downlink
            .dedicated
            .downlink_minimum_offset_db = 0.0;
        config.power_control.downlink.dedicated.tpc_offset_db = 6.0;
        config.power_control.downlink.allocation = Some(RustDownlinkPowerConfig {
            carrier_rms: 0.04,
            cpich_percent: 20.0,
            pich_percent: 2.0,
            dedicated_percent: 63.8,
            hsdpa_percent: 0.0,
            hs_scch_percent: 0.0,
            e_agch_percent: 0.0,
            e_hich_percent: 0.0,
            ..RustDownlinkPowerConfig::default()
        });
        config
    }

    #[test]
    fn component_limit_preserves_the_configured_margin_without_extra_backoff() {
        for margin_db in [0.1, 1.0, 6.0, 20.0] {
            let mut config = profile();
            config
                .power_control
                .downlink
                .allocation
                .as_mut()
                .unwrap()
                .peak_margin_db = margin_db;
            let plan = Plan::resolve(&config).unwrap().unwrap();
            let requested = 10.0_f64.powf(-f64::from(margin_db) / 20.0);
            let actual = f64::from(plan.component_peak_limit);
            assert!(actual < requested);
            assert!(requested - actual <= requested * f64::from(f32::EPSILON));
            assert!(plan.component_peak_limit < 1.0);
            assert!(
                (plan.common.primary.cpich.powi(2) / plan.output_gain.powi(2) - 0.2).abs() < 1e-6
            );
        }
    }

    #[test]
    fn fixed_gain_preserves_pilot_fraction_and_indicator_wire_power() {
        let config = profile();
        let original = serde_json::to_value(&config).unwrap();
        let plan = Plan::resolve(&config).unwrap().unwrap();
        let fraction = plan.common.primary.cpich.powi(2) / plan.output_gain.powi(2);
        assert!((fraction - 0.2).abs() < 1e-6);
        assert!(plan.peak_component_bound < 10.0_f32.powf(-1.0 / 20.0));
        assert_eq!(plan.pich_offset_db, -10);
        assert_eq!(plan.aich_offset_db, -14);
        assert_eq!(plan.dedicated_offset_db, plan.dedicated_minimum_offset_db);
        assert_eq!(serde_json::to_value(&config).unwrap(), original);
        let cell = crate::cell::CellPlan::compile_tagged(
            &config,
            crate::test_support::at(0).epoch,
            crate::cell::BroadcastTags::reference(crate::cell::CellPlan::CURRENT_MIB_VALUE_TAG)
                .unwrap(),
            crate::cell::ReferencePowers {
                cpich_tx_dbm: 10,
                uplink_interference_dbm: -70,
            },
            super::super::indicator_powers(&config, Some(&plan)),
        )
        .unwrap();
        let wire = cell.information().sib5().settings();
        assert_eq!(wire.pich_power_offset_db, plan.pich_offset_db);
        assert_eq!(wire.aich_power_offset_db, plan.aich_offset_db);
    }

    #[test]
    fn oversized_reservations_and_unrepresentable_paging_power_are_rejected() {
        let mut config = profile();
        config
            .power_control
            .downlink
            .allocation
            .as_mut()
            .unwrap()
            .dedicated_percent = 75.0;
        assert!(Plan::resolve(&config).is_err());
        let mut config = profile();
        config
            .power_control
            .downlink
            .allocation
            .as_mut()
            .unwrap()
            .pich_percent = 1.0;
        assert!(Plan::resolve(&config).is_err());
    }

    #[test]
    fn synchronization_share_preserves_broadcast_total_and_other_channel_allocations() {
        let original: Config = serde_yaml::from_str(include_str!(
            "../../../../../rust/test-support/fixtures/config/fixed-overhead-power.yaml"
        ))
        .unwrap();
        let baseline = original.clone();
        let baseline_plan = Plan::resolve(&baseline).unwrap().unwrap();
        for share in [1.0, 5.0, 20.0, 40.0] {
            let mut candidate = original.clone();
            candidate
                .power_control
                .downlink
                .allocation
                .as_mut()
                .unwrap()
                .broadcast_sync_percent = share;
            let plan = Plan::resolve(&candidate).unwrap().unwrap();
            let primary = plan.common.primary;
            let broadcast = primary.pccpch.powi(2) * BCH_DUTY as f32
                + (primary.primary_sch.powi(2) + primary.secondary_sch.powi(2)) * SCH_DUTY as f32;
            let settings = candidate
                .power_control
                .downlink
                .allocation
                .as_ref()
                .unwrap();
            let expected = plan.output_gain.powi(2) * settings.broadcast_percent / PERCENT as f32;
            assert!((broadcast - expected).abs() < 1e-8);
            assert!(
                (primary.primary_sch.powi(2) * SCH_DUTY as f32 * 2.0 / broadcast
                    - share / PERCENT as f32)
                    .abs()
                    < 1e-6
            );
            assert_eq!(primary.primary_sch, primary.secondary_sch);
            assert_eq!(primary.cpich, baseline_plan.common.primary.cpich);
            assert_eq!(plan.common.fach, baseline_plan.common.fach);
            assert_eq!(plan.common.pch, baseline_plan.common.pch);
            assert_eq!(plan.output_gain, baseline_plan.output_gain);
            assert_eq!(
                plan.component_peak_limit,
                baseline_plan.component_peak_limit
            );
            assert_eq!(plan.gamma_db, baseline_plan.gamma_db);
            assert_eq!(plan.hsdpa_power_limit, baseline_plan.hsdpa_power_limit);
            assert_eq!(plan.aich_offset_db, baseline_plan.aich_offset_db);
            assert_eq!(plan.pich_offset_db, baseline_plan.pich_offset_db);
            assert_eq!(plan.dedicated_offset_db, baseline_plan.dedicated_offset_db);
            assert_eq!(
                plan.hs_scch_amplitude.unwrap(),
                baseline_plan.hs_scch_amplitude.unwrap()
            );
            assert_eq!(
                plan.e_agch_amplitude.unwrap(),
                baseline_plan.e_agch_amplitude.unwrap()
            );
            assert_eq!(
                plan.e_hich_amplitude.unwrap(),
                baseline_plan.e_hich_amplitude.unwrap()
            );
        }
    }

    #[test]
    fn absent_budget_preserves_legacy_amplitudes() {
        let mut config = profile();
        config.power_control.downlink.allocation = None;
        let before = serde_json::to_value(&config).unwrap();
        assert!(Plan::resolve(&config).unwrap().is_none());
        assert_eq!(serde_json::to_value(config).unwrap(), before);
    }

    #[test]
    fn hspa_enablement_and_capacity_never_rescale_fixed_references() {
        let source: Config = serde_yaml::from_str(include_str!(
            "../../../../../rust/test-support/fixtures/config/pilot-power-control.yaml"
        ))
        .unwrap();
        for users in [1, 2] {
            for codes in [12, 14] {
                let mut enabled = source.clone();
                enabled.runtime.maximum_ues = users;
                enabled.hspa.downlink.resources.data_codes = codes;
                enabled
                    .power_control
                    .downlink
                    .allocation
                    .as_mut()
                    .unwrap()
                    .carrier_rms = 0.04;
                let mut disabled = enabled.clone();
                disabled.hspa.downlink.enabled = false;
                disabled.hspa.uplink.enabled = false;
                let off = Plan::resolve(&disabled).unwrap().unwrap();
                let on = Plan::resolve(&enabled).unwrap().unwrap();
                assert_eq!(on.output_gain, 0.04);
                assert_eq!(on.output_gain, off.output_gain);
                assert_eq!(on.common.primary.cpich, off.common.primary.cpich);
                assert_eq!(on.common.primary.pccpch, off.common.primary.pccpch);
                assert_eq!(
                    on.common.primary.primary_sch,
                    off.common.primary.primary_sch
                );
                assert_eq!(
                    on.common.primary.secondary_sch,
                    off.common.primary.secondary_sch
                );
                assert_eq!(on.common.fach, off.common.fach);
                assert_eq!(on.common.pch, off.common.pch);
                let reference = on.output_gain.powi(2);
                let check = |actual: f32, expected: f32| {
                    assert!((actual / reference - expected).abs() < 1e-6);
                };
                check(on.common.primary.cpich.powi(2), 0.2);
                check(on.common.primary.pccpch.powi(2) * BCH_DUTY as f32, 0.08);
                check(
                    on.common.primary.primary_sch.powi(2) * SCH_DUTY as f32,
                    0.01,
                );
                check(
                    on.common.primary.secondary_sch.powi(2) * SCH_DUTY as f32,
                    0.01,
                );
                check(on.common.fach.powi(2), 0.016);
                check(on.common.pch.powi(2), 0.016);
                check(
                    on.hsdpa_power_limit.unwrap() * hsdpa_transmission_slots(&enabled) as f32,
                    0.4,
                );
                check(
                    on.hs_scch_amplitude.unwrap().powi(2)
                        * f32::from(enabled.hspa.downlink.resources.control_codes),
                    0.02,
                );
                check(on.e_agch_amplitude.unwrap().powi(2), 0.01);
                check(on.e_hich_amplitude.unwrap().powi(2) * users as f32, 0.01);
                let data = on.common.primary.cpich * 10.0_f32.powf(on.dedicated_offset_db / 20.0);
                let tpc_power = 10.0_f64
                    .powf(f64::from(enabled.power_control.downlink.dedicated.tpc_offset_db) / 10.0);
                check(
                    data.powi(2) * users as f32 * maximum_dpch_average_multiplier(tpc_power) as f32,
                    0.198,
                );
            }
        }
    }

    #[test]
    fn unsafe_fixed_reference_fails_without_mutating_configuration() {
        let mut config = profile();
        config
            .power_control
            .downlink
            .allocation
            .as_mut()
            .unwrap()
            .carrier_rms = 0.75;
        let before = serde_json::to_value(&config).unwrap();
        let error = Plan::resolve(&config).unwrap_err();
        assert!(error.to_string().contains("CPICH amplitude limit"));
        assert_eq!(serde_json::to_value(config).unwrap(), before);
    }

    #[test]
    fn dpch_average_budget_covers_every_normal_slot_without_continuous_peak_backoff() {
        let power = 10.0_f64.powf(6.0 / 10.0);
        let maximum = maximum_dpch_average_multiplier(power);
        assert!((maximum - (1.0 + 0.2 * (power - 1.0))).abs() < 1e-12);
        assert!(maximum < power);
        for number in 0..=LAST_NORMAL_DPCH_FORMAT {
            for tfci in [false, true] {
                if let Ok(format) = umts_dsp::dedicated::DownlinkFormat::new(number, tfci) {
                    let total = format.data_bits()
                        + format.tpc_bits()
                        + format.tfci_bits()
                        + format.pilots().bits();
                    let weighted =
                        (total - format.tpc_bits()) as f64 + format.tpc_bits() as f64 * power;
                    assert!(weighted / total as f64 <= maximum + 1e-12);
                }
            }
        }
    }

    #[test]
    fn full_code_hsdpa_waveform_preserves_reference_with_measured_peak_margin() {
        use num_complex::Complex32;
        use umts_dsp::{
            bits::Bit,
            broadcast::FRAME_BITS,
            common_downlink::CommonDownlink,
            hsdpa::{Modulation, PdschSpreader, ScchSpreader},
            pulse_shaping::PulseShaper,
            scrambling::PrimaryScramblingCode,
        };
        use umts_types::time::CHIPS_PER_SLOT;
        let mut config: Config = serde_yaml::from_str(include_str!(
            "../../../../../rust/test-support/fixtures/config/pilot-power-control.yaml"
        ))
        .unwrap();
        config
            .power_control
            .downlink
            .allocation
            .as_mut()
            .unwrap()
            .carrier_rms = 0.17241344;
        let plan = Plan::resolve(&config).unwrap().unwrap();
        assert_eq!(plan.output_gain, 0.17241344);
        assert!(plan.peak_component_bound > plan.component_peak_limit);
        let primary = PrimaryScramblingCode::new(0).unwrap();
        let common = CommonDownlink::new(primary, plan.common.primary).unwrap();
        let data = PdschSpreader::new(primary, 2, 14).unwrap();
        let control = ScchSpreader::new(primary, 8).unwrap();
        let mut shaper = PulseShaper::new(SamplesPerChip::Two, 32).unwrap();
        let mut chips = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
        let mut samples = vec![Complex32::default(); chips.len() * 2];
        let mut bits = vec![Bit::Zero; 14 * Modulation::Qam16.bits_per_code()];
        let amplitude = (plan.hsdpa_power_limit.unwrap() / 14.0).sqrt();
        let mut random = 0x1234_5678u32;
        let mut peak = 0.0f32;
        for slot in 0..150 {
            if slot % 3 == 0 {
                for bit in &mut bits {
                    random ^= random << 13;
                    random ^= random >> 17;
                    random ^= random << 5;
                    *bit = Bit::from(random & 1 != 0);
                }
            }
            common
                .compose_slot(&[Bit::Zero; FRAME_BITS], slot % 15, &mut chips)
                .unwrap();
            let start = crate::test_support::at((slot / 3 * 3) as u64 * CHIPS_PER_SLOT);
            data.add_slot(
                Modulation::Qam16,
                &bits,
                start,
                slot % 3,
                amplitude,
                &mut chips,
            )
            .unwrap();
            control
                .add_slot(
                    &[Bit::Zero; 120],
                    start,
                    slot % 3,
                    plan.hs_scch_amplitude.unwrap(),
                    &mut chips,
                )
                .unwrap();
            shaper.process(&chips, &mut samples).unwrap();
            for value in &samples {
                assert!(value.re.is_finite() && value.im.is_finite());
                peak = peak.max(value.re.abs()).max(value.im.abs());
            }
        }
        assert!(peak < plan.component_peak_limit);
    }
}
