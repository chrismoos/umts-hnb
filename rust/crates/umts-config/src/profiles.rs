// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::*;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

/// Tested cell policy. The selected recipe is not a universal standards mandate.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Rel99Profile {
    #[default]
    Standard,
}

impl Rel99Profile {
    pub fn planner(self) -> R99PlannerConfig {
        match self {
            Self::Standard => standard_planner(),
        }
    }
}

// TS 26.101 tables 2 and 3. Codec data, independent of the offered mode subset.
const AMR_NB_CLASS_BITS: [[u16; 3]; 8] = [
    [42, 53, 0],
    [49, 54, 0],
    [55, 63, 0],
    [58, 76, 0],
    [61, 87, 0],
    [75, 84, 0],
    [65, 99, 40],
    [81, 103, 60],
];
const AMR_NB_SID_BITS: [u16; 3] = [39, 0, 0];

fn circuit_formats() -> Vec<R99CircuitFormat> {
    [
        [0, 0, 0],
        AMR_NB_SID_BITS,
        AMR_NB_CLASS_BITS[0],
        AMR_NB_CLASS_BITS[2],
        AMR_NB_CLASS_BITS[5],
        AMR_NB_CLASS_BITS[7],
    ]
    .into_iter()
    .map(|bits| R99CircuitFormat {
        subflow_bits: bits.to_vec(),
    })
    .collect()
}

pub(crate) fn standard_planner() -> R99PlannerConfig {
    R99PlannerConfig {
        budgets: R99PlannerBudgets {
            maximum_combinations: 1024,
            maximum_mapping_entries: 1000000,
            maximum_total_coded_bits: 500000,
            maximum_transport_coded_bits: 500000,
            maximum_user_storage_bytes: 16000000,
        },
        physical_candidates: vec![
            R99PhysicalCandidate {
                default_offset_512_chips: 0,
                downlink_code_count: 1,
                downlink_pilot_bits: 8,
                downlink_sf: 256,
                downlink_tfci: true,
                dpcch_offset_db: -80,
                flexible_position: true,
                pc_preamble_frames: 0,
                pilot_offset_quarter_db: 0,
                srb_delay_frames: 0,
                tpc_algorithm: R99TpcAlgorithm::Algorithm2,
                tpc_step_db: 1,
                uplink_dpdch_count: 1,
                uplink_minimum_sf: 4,
                uplink_tfci: true,
            },
            R99PhysicalCandidate {
                default_offset_512_chips: 0,
                downlink_code_count: 1,
                downlink_pilot_bits: 8,
                downlink_sf: 128,
                downlink_tfci: true,
                dpcch_offset_db: -80,
                flexible_position: true,
                pc_preamble_frames: 0,
                pilot_offset_quarter_db: 0,
                srb_delay_frames: 0,
                tpc_algorithm: R99TpcAlgorithm::Algorithm2,
                tpc_step_db: 1,
                uplink_dpdch_count: 1,
                uplink_minimum_sf: 4,
                uplink_tfci: true,
            },
            R99PhysicalCandidate {
                default_offset_512_chips: 0,
                downlink_code_count: 1,
                downlink_pilot_bits: 16,
                downlink_sf: 8,
                downlink_tfci: true,
                dpcch_offset_db: -80,
                flexible_position: true,
                pc_preamble_frames: 0,
                pilot_offset_quarter_db: 0,
                srb_delay_frames: 0,
                tpc_algorithm: R99TpcAlgorithm::Algorithm2,
                tpc_step_db: 1,
                uplink_dpdch_count: 1,
                uplink_minimum_sf: 4,
                uplink_tfci: true,
            },
        ],
        services: R99ServicesConfig {
            circuit: vec![R99CircuitService {
                contract: R99ServiceContract {
                    delivery_order: true,
                    maximum_transfer_delay_ms: Some(80),
                    subflows: vec![
                        R99Reliability {
                            delivery: R99ErrorDelivery::Indicate,
                            residual: R99ErrorRatio {
                                exponent: 6,
                                mantissa: 1,
                            },
                            sdu: Some(R99ErrorRatio {
                                exponent: 5,
                                mantissa: 1,
                            }),
                        },
                        R99Reliability {
                            delivery: R99ErrorDelivery::NoDetection,
                            residual: R99ErrorRatio {
                                exponent: 6,
                                mantissa: 1,
                            },
                            sdu: None,
                        },
                        R99Reliability {
                            delivery: R99ErrorDelivery::NoDetection,
                            residual: R99ErrorRatio {
                                exponent: 6,
                                mantissa: 1,
                            },
                            sdu: None,
                        },
                    ],
                    traffic_class: R99TrafficClass::Conversational,
                },
                formats: circuit_formats(),
                name: "amr-narrowband-core-contract".into(),
                interval_ms: 20,
                subflows: vec![
                    R99CircuitSubflow {
                        downlink: R99Coding {
                            crc_bits: 12,
                            kind: R99CodingKind::ConvolutionalThird,
                            rate_matching_attribute: 200,
                        },
                        empty_block: true,
                        uplink: R99Coding {
                            crc_bits: 12,
                            kind: R99CodingKind::ConvolutionalThird,
                            rate_matching_attribute: 200,
                        },
                    },
                    R99CircuitSubflow {
                        downlink: R99Coding {
                            crc_bits: 0,
                            kind: R99CodingKind::ConvolutionalThird,
                            rate_matching_attribute: 190,
                        },
                        empty_block: false,
                        uplink: R99Coding {
                            crc_bits: 0,
                            kind: R99CodingKind::ConvolutionalThird,
                            rate_matching_attribute: 190,
                        },
                    },
                    R99CircuitSubflow {
                        downlink: R99Coding {
                            crc_bits: 0,
                            kind: R99CodingKind::ConvolutionalHalf,
                            rate_matching_attribute: 235,
                        },
                        empty_block: false,
                        uplink: R99Coding {
                            crc_bits: 0,
                            kind: R99CodingKind::ConvolutionalHalf,
                            rate_matching_attribute: 235,
                        },
                    },
                ],
                versions: 3,
            }],
            packet: vec![R99PacketService {
                am: Some(R99AmPolicy {
                    buffers: R99BufferLimits {
                        maximum_buffer_bytes: 4000000,
                        maximum_sdu_bytes: 2048,
                        queued_sdus: 16,
                    },
                    network: R99NetworkAmPolicy {
                        last_retransmission: true,
                        last_transmission: true,
                        maximum_data_attempts: 10,
                        maximum_reset_attempts: 4,
                        missing_pdu_indicator: true,
                        poll_every_pdus: Some(1),
                        poll_prohibit_ms: None,
                        poll_timeout_ms: Some(200),
                        reset_timeout_ms: 400,
                        status_prohibit_ms: Some(100),
                    },
                    ue: R99AmPeer {
                        downlink_window: 2047,
                        in_sequence_delivery: true,
                        last_retransmission: true,
                        last_transmission: true,
                        maximum_data_attempts: 10,
                        maximum_reset_attempts: 4,
                        missing_pdu_indicator: true,
                        poll_every_pdus: Some(1),
                        poll_prohibit_ms: None,
                        poll_timeout_ms: Some(200),
                        reset_timeout_ms: 400,
                        status_prohibit_ms: Some(20),
                        uplink_window: 2047,
                    },
                }),
                contract: R99ServiceContract {
                    delivery_order: true,
                    maximum_transfer_delay_ms: None,
                    subflows: vec![R99Reliability {
                        delivery: R99ErrorDelivery::Discard,
                        residual: R99ErrorRatio {
                            exponent: 6,
                            mantissa: 1,
                        },
                        sdu: Some(R99ErrorRatio {
                            exponent: 4,
                            mantissa: 1,
                        }),
                    }],
                    traffic_class: R99TrafficClass::Background,
                },
                downlink: R99PacketChannel {
                    coding: R99Coding {
                        crc_bits: 16,
                        kind: R99CodingKind::Turbo,
                        rate_matching_attribute: 200,
                    },
                    interval_ms: 10,
                    maximum_blocks: 3,
                    payload_budget_bits: 256,
                    rlc_bits: 2040,
                },
                name: "background-am-sdu-1e4".into(),
                um: None,
                uplink: R99PacketChannel {
                    coding: R99Coding {
                        crc_bits: 16,
                        kind: R99CodingKind::Turbo,
                        rate_matching_attribute: 200,
                    },
                    interval_ms: 10,
                    maximum_blocks: 4,
                    payload_budget_bits: 928,
                    rlc_bits: 952,
                },
            }],
        },
        signalling: R99SignallingProfile {
            downlink: R99Coding {
                crc_bits: 16,
                kind: R99CodingKind::ConvolutionalThird,
                rate_matching_attribute: 200,
            },
            interval_ms: 40,
            maximum_blocks: 1,
            um_downlink_bits: 240,
            um_uplink_bits: 232,
            uplink: R99Coding {
                crc_bits: 16,
                kind: R99CodingKind::ConvolutionalThird,
                rate_matching_attribute: 200,
            },
        },
        uplink_control_gain: 3,
        uplink_data_gain: 15,
    }
}

/// Expand a named profile before deserialization so advanced partial overrides
/// inherit every unspecified value from the same recipe.
pub(crate) fn resolve(value: &mut Value) -> Result<()> {
    let Some(selected) = value.get("rel99").and_then(|v| v.get("profile")) else {
        return Ok(());
    };
    let profile: Rel99Profile =
        serde_yaml::from_value(selected.clone()).context("rel99.profile")?;
    let mut base = serde_yaml::to_value(profile.planner())?;
    let rel99 = value
        .as_mapping_mut()
        .context("configuration must be a mapping")?
        .entry(Value::from("rel99"))
        .or_insert_with(|| Value::Mapping(Default::default()));
    let physical = rel99
        .as_mapping_mut()
        .context("rel99 must be a mapping")?
        .entry(Value::from("physical"))
        .or_insert_with(|| Value::Mapping(Default::default()));
    let physical = physical
        .as_mapping_mut()
        .context("rel99.physical must be a mapping")?;
    if let Some(overrides) = physical.remove(Value::from("planner")) {
        merge(&mut base, overrides);
    }
    physical.insert(Value::from("planner"), base);
    Ok(())
}

fn merge(base: &mut Value, overrides: Value) {
    match (base, overrides) {
        (Value::Mapping(base), Value::Mapping(overrides)) => {
            for (key, value) in overrides {
                if let Some(previous) = base.get_mut(&key) {
                    merge(previous, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, value) => *base = value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Deployment;

    const DEFAULTS: &str = include_str!("../../../../config/defaults.yaml");
    const EXPANDED: &str =
        include_str!("../../../test-support/fixtures/profiles/standard-reference.yaml");
    const OPERATOR: &str = include_str!("../../../test-support/fixtures/config/operator.yaml");

    #[test]
    fn standard_recipe_preserves_the_expanded_release_profile() {
        let expanded: Value = serde_yaml::from_str(EXPANDED).unwrap();
        assert_eq!(
            serde_yaml::to_value(standard_planner()).unwrap(),
            expanded["rel99"]["physical"]["planner"]
        );
        let mut before = Deployment::parse(EXPANDED, OPERATOR).unwrap();
        assert_eq!(before.config.power_control.uplink.sir_ceiling_db, 13.0);
        before.effective["power_control"]["uplink"]["sir_ceiling_db"] = Value::from(16.0);
        let after = Deployment::parse(DEFAULTS, OPERATOR).unwrap();
        assert_eq!(before.effective, after.effective);
    }

    #[test]
    fn advanced_overrides_inherit_the_rest_of_the_selected_profile() {
        let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
        operator["rel99"] = serde_yaml::from_str("profile: standard\nphysical:\n  planner:\n    signalling:\n      um_downlink_bits: 248\n    budgets:\n      maximum_user_storage_bytes: 32000000\n").unwrap();
        let loaded =
            Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).unwrap();
        let mut expected = standard_planner();
        expected.signalling.um_downlink_bits = 248;
        expected.budgets.maximum_user_storage_bytes = 32000000;
        assert_eq!(
            serde_yaml::to_value(loaded.config.rel99.physical.planner).unwrap(),
            serde_yaml::to_value(expected).unwrap()
        );
    }

    #[test]
    fn unified_configuration_without_a_profile_is_left_unchanged() {
        let mut value: Value = serde_yaml::from_str(EXPANDED).unwrap();
        value["rel99"]
            .as_mapping_mut()
            .unwrap()
            .remove(Value::from("profile"));
        let original = value.clone();
        resolve(&mut value).unwrap();
        assert_eq!(value, original);
    }

    #[test]
    fn unknown_profiles_and_malformed_overrides_are_rejected() {
        for fragment in [
            "profile: unknown",
            "profile: null",
            "physical:\n  planner: null",
            "physical:\n  planner:\n    signalling:\n      unknown: 1",
        ] {
            let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
            operator["rel99"] = serde_yaml::from_str(fragment).unwrap();
            assert!(
                Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).is_err(),
                "{fragment}"
            );
        }
    }
    #[test]
    fn removed_controls_cannot_be_silently_accepted() {
        let original: Value = serde_yaml::from_str(EXPANDED).unwrap();
        for path in [
            "cell.common_channels.primary_cpich_channel_estimation",
            "rel99.packet_capacity_bytes_per_second",
            "rel99.cs_ps_combined_data",
            "rel99.physical.packet_turbo_codes",
            "rel99.physical.uplink_gain_mode",
            "rel99.physical.uplink_reference_profile",
            "rel99.likelihood_decode",
            "rel99.temporal_timing",
            "rel99.tfci_likelihood",
            "rel99.oversampled_payload",
            "rel99.pilot_fit_payload",
            "runtime.transmit.tx_latency_slots",
            "runtime.transmit.composition_lead_slots",
            "runtime.transmit.commit_lead_slots",
            "runtime.transmit.scheduler_lead_frames",
            "receiver.access.rach_search_window",
            "receiver.access.rach_pilot_search_chips",
            "receiver.access.rach_detection_threshold",
            "receiver.access.max_expected_delay_spread_chips",
            "receiver.hspa.uplink_pilot_fit_payload",
            "receiver.hspa.uplink_fullrate_receiver",
            "receiver.hspa.uplink_fullrate_sinc",
            "receiver.hspa.uplink_causal_timing",
            "hspa.downlink.max_cqi",
            "hspa.downlink.max_tfri",
            "hspa.downlink.demand_tfri_format",
            "hspa.downlink.dl_rlc_pdu_bits",
            "hspa.downlink.dl_rlc_buffer_bytes",
            "hspa.downlink.rlc_poll_prohibit_ms",
            "hspa.uplink.edch_rot_limit_enable",
            "hspa.uplink.non_scheduled_pdus",
            "power_control.uplink.tpc_target_pilot_dbfs",
            "hspa.downlink.fixed_tfri_format",
        ] {
            let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
            let keys: Vec<_> = path.split('.').collect();
            let mut cursor = &mut operator;
            for key in &keys[..keys.len() - 1] {
                if cursor.get(*key).is_none() {
                    cursor[*key] = Value::Mapping(Default::default());
                }
                cursor = &mut cursor[*key];
            }
            cursor[*keys.last().unwrap()] = Value::Null;
            let error = Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap())
                .err()
                .expect(path);
            assert!(
                format!("{error:#}").contains("unknown configuration property"),
                "{path}: {error:#}"
            );
            // Direct unified-file deserialization must reject the removed property too.
            let mut unified = original.clone();
            let mut cursor = &mut unified;
            for key in &keys[..keys.len() - 1] {
                if cursor.get(*key).is_none() {
                    cursor[*key] = Value::Mapping(Default::default());
                }
                cursor = &mut cursor[*key];
            }
            cursor[*keys.last().unwrap()] = Value::Null;
            // Supply the required operator settings before checking unknown fields.
            let mut base =
                serde_yaml::to_value(Deployment::parse(DEFAULTS, OPERATOR).unwrap().config)
                    .unwrap();
            merge(&mut base, unified);
            assert!(
                serde_yaml::from_value::<Config>(base).is_err(),
                "accepted {path}"
            );
        }
    }
}
