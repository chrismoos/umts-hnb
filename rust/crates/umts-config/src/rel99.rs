// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{R99Coding, R99PhysicalCandidate, R99PlannerConfig, R99TpcAlgorithm};
use anyhow::{Context, Result, ensure};

const MAX_CANDIDATES: usize = 16;
const MAX_FORMATS: u16 = 32;
const MAX_BLOCKS: u16 = MAX_FORMATS - 1;
const MAX_CODES: u8 = 8;
const MAX_DOFF: u16 = 599;
const MAX_PILOT_OFFSET: u8 = 24;
const OCTET_BITS: u16 = 8;
const MIN_UM_RECEIVE_BITS: u16 = 2 * OCTET_BITS;
const MIN_UM_TRANSMIT_BITS: u16 = 3 * OCTET_BITS;

impl R99Coding {
    fn validate_policy(&self) -> Result<()> {
        ensure!(
            matches!(self.crc_bits, 0 | 8 | 12 | 16 | 24),
            "crc_bits must be 0, 8, 12, 16 or 24"
        );
        ensure!(
            (1..=256).contains(&self.rate_matching_attribute),
            "rate_matching_attribute must be in 1..=256"
        );
        Ok(())
    }
}

impl R99PlannerConfig {
    pub fn validate_policy(&self) -> Result<()> {
        self.validate_schema()?;
        let signalling = &self.signalling;
        signalling
            .uplink
            .validate_policy()
            .context("rel99.physical.planner.signalling.uplink")?;
        signalling
            .downlink
            .validate_policy()
            .context("rel99.physical.planner.signalling.downlink")?;
        ensure!(
            matches!(signalling.interval_ms, 10 | 20 | 40 | 80),
            "rel99.physical.planner.signalling.interval_ms must be 10, 20, 40 or 80"
        );
        ensure!(
            (1..=MAX_BLOCKS).contains(&signalling.maximum_blocks),
            "rel99.physical.planner.signalling.maximum_blocks must be in 1..={MAX_BLOCKS}"
        );
        ensure!(
            signalling.um_uplink_bits >= MIN_UM_RECEIVE_BITS
                && signalling.um_uplink_bits.is_multiple_of(OCTET_BITS),
            "rel99.physical.planner.signalling.um_uplink_bits must be octet-aligned and at least {MIN_UM_RECEIVE_BITS}"
        );
        ensure!(
            signalling.um_downlink_bits >= MIN_UM_TRANSMIT_BITS
                && signalling.um_downlink_bits.is_multiple_of(OCTET_BITS),
            "rel99.physical.planner.signalling.um_downlink_bits must be octet-aligned and at least {MIN_UM_TRANSMIT_BITS}"
        );
        ensure!(
            !self.physical_candidates.is_empty()
                && self.physical_candidates.len() <= MAX_CANDIDATES,
            "rel99.physical.planner.physical_candidates must contain 1..={MAX_CANDIDATES} entries"
        );
        for (index, candidate) in self.physical_candidates.iter().enumerate() {
            candidate
                .validate_policy()
                .with_context(|| format!("rel99.physical.planner.physical_candidates[{index}]"))?;
        }
        // TS 25.213 4.2.1 normalizes at least one beta factor to one (quantized index 15).
        ensure!(
            (1..=15).contains(&self.uplink_control_gain)
                && (1..=15).contains(&self.uplink_data_gain)
                && self.uplink_control_gain.max(self.uplink_data_gain) == 15,
            "rel99.physical.planner uplink gains must be in 1..=15 with at least one equal to 15"
        );
        Ok(())
    }
}

impl R99PhysicalCandidate {
    fn validate_policy(&self) -> Result<()> {
        ensure!(
            matches!(self.uplink_minimum_sf, 4 | 8 | 16 | 32 | 64 | 128 | 256),
            "uplink_minimum_sf must be a legal uplink spreading factor"
        );
        ensure!(
            (1..=6).contains(&self.uplink_dpdch_count)
                && (self.uplink_dpdch_count == 1 || self.uplink_minimum_sf == 4),
            "multiple uplink DPDCHs require spreading factor 4"
        );
        ensure!(
            (-164..=-6).contains(&self.dpcch_offset_db) && self.dpcch_offset_db % 2 == 0,
            "dpcch_offset_db must be -164..=-6 in 2 dB steps"
        );
        ensure!(
            self.pc_preamble_frames <= 7 && self.srb_delay_frames <= 7,
            "preamble and SRB delay must be at most 7 frames"
        );
        ensure!(
            matches!(self.tpc_algorithm, R99TpcAlgorithm::Algorithm2)
                || matches!(self.tpc_step_db, 1 | 2),
            "algorithm-1 tpc_step_db must be 1 or 2"
        );
        let valid_pilot = matches!(
            (self.downlink_sf, self.downlink_pilot_bits),
            (4 | 8 | 16, 16) | (32 | 64, 8) | (128, 4 | 8) | (256, 2 | 4 | 8) | (512, 4)
        );
        ensure!(
            valid_pilot,
            "downlink_sf and downlink_pilot_bits must select a legal normal DPCH slot format"
        );
        ensure!(
            (1..=MAX_CODES).contains(&self.downlink_code_count)
                && u16::from(self.downlink_code_count) <= self.downlink_sf,
            "downlink_code_count exceeds the normal DPCH code budget"
        );
        ensure!(
            self.pilot_offset_quarter_db <= MAX_PILOT_OFFSET,
            "pilot_offset_quarter_db must be in 0..={MAX_PILOT_OFFSET}"
        );
        ensure!(
            self.default_offset_512_chips <= MAX_DOFF,
            "default_offset_512_chips must be in 0..={MAX_DOFF}"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_yaml_overrides_preserve_named_defaults() {
        let config: R99PlannerConfig = serde_yaml::from_str("signalling:\n  interval_ms: 20\nuplink_data_gain: 7\nbudgets:\n  maximum_combinations: 256\n").unwrap();
        config.validate_policy().unwrap();
        assert_eq!(config.signalling.interval_ms, 20);
        assert_eq!(config.signalling.um_uplink_bits, 144);
        assert_eq!(config.signalling.um_downlink_bits, 336);
        assert_eq!(config.uplink_data_gain, 7);
        assert_eq!(config.uplink_control_gain, 15);
        assert_eq!(config.budgets.maximum_combinations, 256);
        assert_eq!(config.budgets.maximum_mapping_entries, 1_000_000);
        assert_eq!(config.physical_candidates.len(), 1);
    }

    #[test]
    fn nested_unknown_keys_and_invalid_enum_tokens_are_not_ignored() {
        for yaml in [
            "unknown: 1",
            "signalling: {interval: 40}",
            "budgets: {maximum_total_code_bits: 1}",
            "physical_candidates: [{downlink_codes: [3]}]",
            "signalling: {uplink: {kind: unknown}}",
        ] {
            assert!(
                serde_yaml::from_str::<R99PlannerConfig>(yaml).is_err(),
                "accepted {yaml}"
            );
        }
    }

    #[test]
    fn physical_list_and_gain_budgets_are_bounded() {
        let mut config = R99PlannerConfig::default();
        let candidate = config.physical_candidates[0].clone();
        config.physical_candidates = vec![candidate; MAX_CANDIDATES];
        config.validate_policy().unwrap();
        config
            .physical_candidates
            .push(config.physical_candidates[0].clone());
        assert!(config.validate_policy().is_err());
        config.physical_candidates.truncate(1);
        for (control, data) in [(0, 15), (15, 0), (16, 15), (15, 16), (7, 7)] {
            config.uplink_control_gain = control;
            config.uplink_data_gain = data;
            assert!(config.validate_policy().is_err());
        }
    }

    #[test]
    fn legal_normal_dpch_pilots_and_multicode_constraints_are_distinct() {
        let mut config = R99PlannerConfig::default();
        for (sf, pilots) in [
            (4, 16),
            (8, 16),
            (16, 16),
            (32, 8),
            (64, 8),
            (128, 4),
            (128, 8),
            (256, 2),
            (256, 4),
            (256, 8),
            (512, 4),
        ] {
            let candidate = &mut config.physical_candidates[0];
            candidate.downlink_sf = sf;
            candidate.downlink_pilot_bits = pilots;
            config.validate_policy().unwrap();
        }
        config.physical_candidates[0].uplink_dpdch_count = 6;
        config.physical_candidates[0].uplink_minimum_sf = 4;
        config.validate_policy().unwrap();
        config.physical_candidates[0].uplink_minimum_sf = 8;
        assert!(config.validate_policy().is_err());
    }
}
