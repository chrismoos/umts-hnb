// SPDX-License-Identifier: AGPL-3.0-or-later
use crate::{NetworkOperationMode, PacketMode};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(super) fn default_scheduling_enabled() -> bool {
    true
}
pub(super) fn default_release_rab_on_error_indication() -> bool {
    true
}
pub(super) fn default_usb_priority() -> u8 {
    90
}
pub(super) fn default_io_priority() -> u8 {
    85
}
pub(super) fn default_prepare_priority() -> u8 {
    80
}
pub(super) fn default_receive_priority() -> u8 {
    75
}
pub(super) fn default_ue_measurement_report_count() -> Option<u8> {
    Some(32)
}
pub(super) fn default_ue_measurement_interval_ms() -> u32 {
    1000
}
pub(super) fn default_security_algorithm_enabled() -> bool {
    true
}
pub(super) fn default_management_enabled() -> bool {
    true
}
pub(super) fn default_management_socket() -> PathBuf {
    crate::paths::default_runtime_path("management.sock")
}
pub(super) fn default_management_administrator_socket() -> PathBuf {
    crate::paths::default_runtime_path("admin.sock")
}
pub(super) fn default_management_capture_directory() -> PathBuf {
    crate::paths::default_runtime_path("captures")
}
pub(super) fn default_management_capture_disk_mib() -> u32 {
    2048
}
pub(super) fn default_hsdpa_preparation_guard_us() -> u32 {
    20000
}
pub(super) fn default_hsdpa_cqi_offset_limit() -> u8 {
    0
}
pub(super) fn default_hsdpa_cqi_target_bler_percent() -> u8 {
    10
}
pub(super) fn default_r99_circuit_services() -> Vec<R99CircuitService> {
    vec![R99CircuitService {
        name: "amr-narrowband".to_owned(),
        contract: R99ServiceContract {
            traffic_class: R99TrafficClass::Conversational,
            delivery_order: true,
            maximum_transfer_delay_ms: Some(80),
            subflows: vec![
                R99Reliability {
                    residual: R99ErrorRatio {
                        mantissa: 1,
                        exponent: 6,
                    },
                    sdu: Some(R99ErrorRatio {
                        mantissa: 1,
                        exponent: 3,
                    }),
                    delivery: R99ErrorDelivery::Indicate,
                },
                R99Reliability {
                    residual: R99ErrorRatio {
                        mantissa: 1,
                        exponent: 6,
                    },
                    sdu: None,
                    delivery: R99ErrorDelivery::NoDetection,
                },
                R99Reliability {
                    residual: R99ErrorRatio {
                        mantissa: 1,
                        exponent: 6,
                    },
                    sdu: None,
                    delivery: R99ErrorDelivery::NoDetection,
                },
            ],
        },
        interval_ms: 20,
        subflows: vec![
            R99CircuitSubflow {
                uplink: R99Coding {
                    kind: R99CodingKind::ConvolutionalThird,
                    crc_bits: 12,
                    rate_matching_attribute: 200,
                },
                downlink: R99Coding {
                    kind: R99CodingKind::ConvolutionalThird,
                    crc_bits: 12,
                    rate_matching_attribute: 200,
                },
                empty_block: true,
            },
            R99CircuitSubflow {
                uplink: R99Coding {
                    kind: R99CodingKind::ConvolutionalThird,
                    crc_bits: 0,
                    rate_matching_attribute: 190,
                },
                downlink: R99Coding {
                    kind: R99CodingKind::ConvolutionalThird,
                    crc_bits: 0,
                    rate_matching_attribute: 190,
                },
                empty_block: false,
            },
            R99CircuitSubflow {
                uplink: R99Coding {
                    kind: R99CodingKind::ConvolutionalHalf,
                    crc_bits: 0,
                    rate_matching_attribute: 235,
                },
                downlink: R99Coding {
                    kind: R99CodingKind::ConvolutionalHalf,
                    crc_bits: 0,
                    rate_matching_attribute: 235,
                },
                empty_block: false,
            },
        ],
        formats: vec![
            R99CircuitFormat {
                subflow_bits: vec![0, 0, 0],
            },
            R99CircuitFormat {
                subflow_bits: vec![39, 0, 0],
            },
            R99CircuitFormat {
                subflow_bits: vec![42, 53, 0],
            },
            R99CircuitFormat {
                subflow_bits: vec![55, 63, 0],
            },
            R99CircuitFormat {
                subflow_bits: vec![75, 84, 0],
            },
            R99CircuitFormat {
                subflow_bits: vec![81, 103, 60],
            },
        ],
        versions: 3,
    }]
}
pub(super) fn default_r99_packet_services() -> Vec<R99PacketService> {
    vec![R99PacketService {
        name: "background-am".to_owned(),
        contract: R99ServiceContract {
            traffic_class: R99TrafficClass::Background,
            delivery_order: true,
            maximum_transfer_delay_ms: None,
            subflows: vec![R99Reliability {
                residual: R99ErrorRatio {
                    mantissa: 1,
                    exponent: 6,
                },
                sdu: Some(R99ErrorRatio {
                    mantissa: 1,
                    exponent: 3,
                }),
                delivery: R99ErrorDelivery::Discard,
            }],
        },
        uplink: R99PacketChannel {
            interval_ms: 20,
            rlc_bits: 320,
            payload_budget_bits: 256,
            maximum_blocks: 31,
            coding: R99Coding {
                kind: R99CodingKind::Turbo,
                crc_bits: 16,
                rate_matching_attribute: 200,
            },
        },
        downlink: R99PacketChannel {
            interval_ms: 20,
            rlc_bits: 320,
            payload_budget_bits: 256,
            maximum_blocks: 31,
            coding: R99Coding {
                kind: R99CodingKind::Turbo,
                crc_bits: 16,
                rate_matching_attribute: 200,
            },
        },
        am: Some(R99AmPolicy {
            ue: R99AmPeer {
                uplink_window: 512,
                downlink_window: 512,
                maximum_data_attempts: 10,
                maximum_reset_attempts: 4,
                reset_timeout_ms: 400,
                poll_timeout_ms: Some(200),
                poll_prohibit_ms: None,
                poll_every_pdus: Some(1),
                last_transmission: true,
                last_retransmission: true,
                status_prohibit_ms: Some(100),
                missing_pdu_indicator: true,
                in_sequence_delivery: true,
            },
            network: R99NetworkAmPolicy {
                maximum_data_attempts: 10,
                maximum_reset_attempts: 4,
                reset_timeout_ms: 400,
                poll_timeout_ms: Some(200),
                poll_prohibit_ms: None,
                poll_every_pdus: Some(1),
                last_transmission: true,
                last_retransmission: true,
                status_prohibit_ms: Some(100),
                missing_pdu_indicator: true,
            },
            buffers: R99BufferLimits {
                maximum_sdu_bytes: 65535,
                queued_sdus: 16,
                maximum_buffer_bytes: 4000000,
            },
        }),
        um: None,
    }]
}
pub(super) fn default_gprs_enable() -> bool {
    true
}
pub(super) fn default_nmo() -> NetworkOperationMode {
    NetworkOperationMode::SeparatePaging
}
pub(super) fn default_hnbgw_port() -> u16 {
    29169
}
pub(super) fn default_gtpu_bind_port() -> u16 {
    2152
}
pub(super) fn default_tx_samples_per_chip() -> u8 {
    2
}
pub(super) fn default_tx_analog_bandwidth_hz() -> u32 {
    5000000
}
pub(super) fn default_bladerf_stream_buffers() -> u32 {
    8
}
pub(super) fn default_bladerf_stream_buffer_samples() -> u32 {
    4096
}
pub(super) fn default_bladerf_stream_transfers() -> u32 {
    4
}
pub(super) fn default_pich_power_offset_db() -> i8 {
    -10
}
pub(super) fn default_turbo_iterations() -> u32 {
    4
}
pub(super) fn default_downlink_scrambling_code() -> u16 {
    469
}
pub(super) fn default_hspa_measurement_power_offset_db() -> Option<f64> {
    None
}
pub(super) fn default_hspa_hs_dpcch_ack_nack_delta() -> u8 {
    3
}
pub(super) fn default_hspa_hs_dpcch_cqi_delta() -> u8 {
    8
}
pub(super) fn default_hspa_cqi_feedback_cycle_ms() -> u16 {
    2
}
pub(super) fn default_hspa_uplink_dpcch_sir_target_db() -> f64 {
    34.7712
}
pub(super) fn default_hspa_uplink_target_bler() -> f64 {
    0.02
}
pub(super) fn default_uplink_dpcch_tpc_hold_groups() -> u8 {
    2
}
pub(super) fn default_hspa_harq_processes() -> u8 {
    6
}
pub(super) fn default_hspa_rlc_pdu_bits() -> u16 {
    4640
}
pub(super) fn default_hspa_rlc_receive_window() -> u16 {
    768
}
pub(super) fn default_hsupa_max_category() -> u8 {
    1
}
pub(super) fn default_hsupa_serving_grant() -> u8 {
    31
}
pub(super) fn default_hsupa_edpcch_power_offset() -> u8 {
    5
}
pub(super) fn default_hsupa_edpdch_references() -> Vec<EdpdchReference> {
    vec![
        EdpdchReference {
            etfci: 11,
            power_offset: 4,
        },
        EdpdchReference {
            etfci: 83,
            power_offset: 17,
        },
    ]
}
pub(super) fn default_serving_maximum_ues() -> u32 {
    16
}
pub(super) fn default_serving_maximum_sdu_bytes() -> u32 {
    8192
}
pub(super) fn default_serving_queued_sdus_per_bearer() -> u32 {
    8
}
pub(super) fn default_serving_maximum_queue_bytes() -> u32 {
    67108864
}
pub(super) fn default_dedicated_shared_bytes() -> u32 {
    4194304
}
pub(super) fn default_dedicated_sample_bytes() -> u32 {
    67108864
}
pub(super) fn default_dedicated_coded_bits() -> u32 {
    8388608
}
pub(super) fn default_dedicated_output_bytes() -> u32 {
    8388608
}
pub(super) fn default_dedicated_demultiplexed_bits() -> u32 {
    8388608
}
pub(super) fn default_dedicated_bearer_bytes() -> u32 {
    67108864
}
pub(super) fn default_dedicated_transmit_bits() -> u32 {
    16777216
}
pub(super) fn default_dedicated_frame_bits() -> u32 {
    262144
}
pub(super) fn default_dedicated_downlink_offset_db() -> f32 {
    0.0
}
pub(super) fn default_dedicated_downlink_minimum_offset_db() -> f32 {
    -12.0
}
pub(super) fn default_dedicated_tpc_offset_db() -> f32 {
    0.0
}
pub(super) fn default_dedicated_history_frames() -> u8 {
    2
}
pub(super) fn default_dedicated_work_per_cycle() -> u16 {
    16
}
pub(super) fn default_dedicated_command_capacity() -> u16 {
    64
}
pub(super) fn default_dedicated_output_slots() -> u16 {
    8
}
pub(super) fn default_dedicated_search_radius_samples() -> u16 {
    8
}
pub(super) fn default_dedicated_minimum_coherent_fraction() -> f64 {
    0.05
}
pub(super) fn default_dedicated_carrier_initial_hz() -> f64 {
    0.0
}
pub(super) fn default_dedicated_carrier_maximum_absolute_hz() -> f64 {
    2000.0
}
pub(super) fn default_dedicated_carrier_update_fraction() -> f64 {
    0.5
}
pub(super) fn default_dedicated_carrier_minimum_coherence() -> f64 {
    0.8
}
pub(super) fn default_dedicated_likelihood_maximum() -> f64 {
    128.0
}
pub(super) fn default_dedicated_tfci_minimum_score() -> f64 {
    0.5
}
pub(super) fn default_dedicated_tfci_minimum_margin() -> f64 {
    0.1
}
pub(super) fn default_dedicated_tpc_maximum_observation_age_chips() -> u32 {
    10240
}
pub(super) fn default_dedicated_tpc_initial_maximum_slots() -> u32 {
    1500
}
pub(super) fn default_dedicated_radio_link_out_of_sync_frames() -> u32 {
    50
}
pub(super) fn default_dedicated_radio_link_in_sync_frames() -> u32 {
    10
}
pub(super) fn default_dedicated_radio_link_failure_frames() -> u32 {
    300
}
pub(super) fn default_downlink_power_carrier_rms() -> f32 {
    0.04
}
pub(super) fn default_downlink_power_peak_margin_db() -> f32 {
    1.0
}
pub(super) fn default_downlink_power_cpich_percent() -> f32 {
    10.0
}
pub(super) fn default_downlink_power_broadcast_percent() -> f32 {
    10.0
}
pub(super) fn default_downlink_power_broadcast_sync_percent() -> f32 {
    20.0
}
pub(super) fn default_downlink_power_fach_percent() -> f32 {
    1.6
}
pub(super) fn default_downlink_power_pch_percent() -> f32 {
    1.6
}
pub(super) fn default_downlink_power_pich_percent() -> f32 {
    1.0
}
pub(super) fn default_downlink_power_aich_percent() -> f32 {
    1.0
}
pub(super) fn default_downlink_power_dedicated_percent() -> f32 {
    29.8
}
pub(super) fn default_downlink_power_hsdpa_percent() -> f32 {
    40.0
}
pub(super) fn default_downlink_power_hs_scch_percent() -> f32 {
    2.0
}
pub(super) fn default_downlink_power_e_agch_percent() -> f32 {
    1.0
}
pub(super) fn default_downlink_power_e_hich_percent() -> f32 {
    1.0
}
pub(super) fn default_serving_cpich_amplitude() -> f32 {
    0.1
}
pub(super) fn default_broadcast_state_file() -> PathBuf {
    crate::paths::default_runtime_path("cell-broadcast.pb")
}
pub(super) fn default_serving_common_channel_offset_db() -> f32 {
    0.0
}
pub(super) fn default_serving_filter_span_chips() -> u16 {
    16
}
pub(super) fn default_serving_rx_timeout_us() -> u32 {
    1000
}
pub(super) fn default_serving_rx_queue_blocks() -> u16 {
    64
}
pub(super) fn default_serving_startup_lead_ms() -> u32 {
    250
}
pub(super) fn default_serving_tx_batch_slots() -> u8 {
    1
}
pub(super) fn default_serving_minimum_submission_us() -> u32 {
    100
}
pub(super) fn default_serving_maximum_submission_us() -> u32 {
    1000
}
pub(super) fn default_serving_maximum_preparation_us() -> u32 {
    1200
}
pub(super) fn default_serving_idle_wait_us() -> u32 {
    100
}
pub(super) fn default_serving_controller_idle_wait_us() -> Option<u32> {
    None
}
pub(super) fn default_serving_startup_timeout_ms() -> u32 {
    5000
}
pub(super) fn default_serving_clock_stall_timeout_ms() -> u32 {
    1000
}
pub(super) fn default_serving_recovery_maximum_restarts() -> u16 {
    3
}
pub(super) fn default_serving_recovery_backoff_ms() -> u32 {
    1000
}
pub(super) fn default_serving_recovery_maximum_backoff_ms() -> u32 {
    30000
}
pub(super) fn default_clock_sampling_interval_us() -> u32 {
    1000000
}
pub(super) fn default_clock_sampling_maximum_age_us() -> Option<u32> {
    None
}
pub(super) fn default_clock_sampling_maximum_query_us() -> u32 {
    100
}
pub(super) fn default_clock_sampling_maximum_drift_ppm() -> u32 {
    100
}
pub(super) fn default_media_packet_bytes() -> u32 {
    8388608
}
pub(super) fn default_serving_hsdpa_data_codes() -> u8 {
    5
}
pub(super) fn default_serving_hsdpa_control_codes() -> u8 {
    1
}
pub(super) fn default_serving_hsdpa_pending() -> u8 {
    32
}
pub(super) fn default_serving_hsdpa_tx_bytes() -> u32 {
    33554432
}
pub(super) fn default_serving_hsdpa_queue_pdus() -> u16 {
    256
}
pub(super) fn default_serving_hsdpa_ue_queue_bytes() -> u32 {
    1048576
}
pub(super) fn default_serving_hsdpa_queue_bytes() -> u32 {
    67108864
}
pub(super) fn default_media_voice_bytes() -> u32 {
    33554432
}
pub(super) fn default_circuit_payload_type() -> u8 {
    96
}
pub(super) fn default_circuit_datagram_bytes() -> u16 {
    2048
}
pub(super) fn default_circuit_wait_ms() -> u32 {
    40
}
pub(super) fn default_circuit_playout_lead_ms() -> u32 {
    40
}
pub(super) fn default_circuit_horizon_ms() -> u32 {
    60000
}
pub(super) fn default_circuit_versions() -> u16 {
    3
}
pub(super) fn default_circuit_timeout_ms() -> u32 {
    400
}
pub(super) fn default_circuit_repetitions() -> u8 {
    3
}
pub(super) fn default_serving_join_timeout_ms() -> u32 {
    3000
}
pub(super) fn default_true() -> bool {
    true
}
pub(super) fn default_backend() -> String {
    "soapy".to_owned()
}
pub(super) fn default_retry_attempts() -> u32 {
    0
}
pub(super) fn default_retry_backoff_secs() -> u64 {
    5
}
pub(super) fn default_log_filter() -> String {
    "info".to_owned()
}
pub(super) fn default_rrc_srnc_id() -> u16 {
    0
}
pub(super) fn default_rrc_ura_identity() -> u16 {
    100
}
pub(super) fn default_rrc_cn_drx_cycle_length_coefficient() -> u8 {
    6
}
pub(super) fn default_rrc_periodic_lu_timer_value() -> u8 {
    30
}
pub(super) fn default_rrc_max_allowed_ul_tx_power_dbm() -> i8 {
    33
}
pub(super) fn default_rrc_inactivity_release_seconds() -> u32 {
    180
}
pub(super) fn default_rrc_release_complete_retransmissions() -> u8 {
    1
}
pub(super) fn default_rrc_inactivity_delete_seconds() -> u32 {
    300
}
pub(super) fn default_rrc_combined_bearer_setup() -> bool {
    true
}
pub(super) fn default_prach_signature() -> u8 {
    13
}
pub(super) fn default_prach_initial_power_constant_db() -> i8 {
    -10
}
pub(super) fn default_prach_subchannel() -> u8 {
    1
}
pub(super) fn default_prach_scrambling_code_word_number() -> u8 {
    0
}
pub(super) fn default_prach_dynamic_persistence_level() -> u8 {
    1
}
pub(super) fn default_common_aich_power_offset_db() -> i8 {
    -10
}
pub(super) fn default_r99_planner_crc_bits() -> u8 {
    16
}
pub(super) fn default_r99_planner_rate_matching_attribute() -> u16 {
    200
}
pub(super) fn default_r99_planner_interval_ms() -> u8 {
    40
}
pub(super) fn default_r99_planner_maximum_blocks() -> u16 {
    1
}
pub(super) fn default_r99_planner_um_uplink_bits() -> u16 {
    144
}
pub(super) fn default_r99_planner_um_downlink_bits() -> u16 {
    336
}
pub(super) fn default_r99_planner_uplink_gain() -> u8 {
    15
}
pub(super) fn default_r99_planner_uplink_minimum_sf() -> u16 {
    4
}
pub(super) fn default_r99_planner_uplink_dpdch_count() -> u8 {
    1
}
pub(super) fn default_r99_planner_tfci() -> bool {
    true
}
pub(super) fn default_r99_planner_dpcch_offset_db() -> i16 {
    -6
}
pub(super) fn default_r99_planner_frame_delay() -> u8 {
    0
}
pub(super) fn default_r99_planner_tpc_step_db() -> u8 {
    1
}
pub(super) fn default_r99_planner_downlink_sf() -> u16 {
    8
}
pub(super) fn default_r99_planner_downlink_pilot_bits() -> u8 {
    16
}
pub(super) fn default_r99_planner_downlink_code_count() -> u8 {
    1
}
pub(super) fn default_r99_planner_flexible_position() -> bool {
    true
}
pub(super) fn default_r99_planner_pilot_offset_quarter_db() -> u8 {
    0
}
pub(super) fn default_r99_planner_offset_512_chips() -> u16 {
    0
}
pub(super) fn default_r99_planner_maximum_combinations() -> u16 {
    1024
}
pub(super) fn default_r99_planner_maximum_total_coded_bits() -> u32 {
    500000
}
pub(super) fn default_r99_planner_maximum_user_storage_bytes() -> u32 {
    16000000
}
pub(super) fn default_r99_planner_maximum_transport_coded_bits() -> u32 {
    500000
}
pub(super) fn default_r99_planner_maximum_mapping_entries() -> u32 {
    1000000
}
pub(super) fn default_r99_planner_physical_candidates() -> Vec<R99PhysicalCandidate> {
    vec![R99PhysicalCandidate {
        uplink_minimum_sf: 4,
        uplink_dpdch_count: 1,
        uplink_tfci: true,
        dpcch_offset_db: -6,
        pc_preamble_frames: 0,
        srb_delay_frames: 0,
        tpc_algorithm: R99TpcAlgorithm::Algorithm1,
        tpc_step_db: 1,
        downlink_sf: 8,
        downlink_pilot_bits: 16,
        downlink_tfci: true,
        downlink_code_count: 1,
        flexible_position: true,
        pilot_offset_quarter_db: 0,
        default_offset_512_chips: 0,
    }]
}
pub(super) fn default_r99_initial_ul_puncturing_limit_percent() -> u8 {
    100
}
pub(super) fn default_r99_uplink_scrambling_code_base() -> u32 {
    12084932
}
pub(super) fn default_q_qual_min() -> i8 {
    -24
}
pub(super) fn default_secondary_carrier_power_percent() -> f64 {
    50.0
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum R99ErrorDelivery {
    #[serde(rename = "indicate")]
    Indicate,
    #[serde(rename = "discard")]
    Discard,
    #[serde(rename = "no-detection")]
    NoDetection,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum R99TrafficClass {
    #[serde(rename = "conversational")]
    Conversational,
    #[serde(rename = "streaming")]
    Streaming,
    #[serde(rename = "interactive")]
    Interactive,
    #[serde(rename = "background")]
    Background,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99ErrorRatio {
    pub mantissa: u8,
    pub exponent: u8,
}
impl R99ErrorRatio {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(1..=9).contains(&self.mantissa) {
            anyhow::bail!("R99ErrorRatio.mantissa outside 1..=9");
        }
        if !(1..=9).contains(&self.exponent) {
            anyhow::bail!("R99ErrorRatio.exponent outside 1..=9");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99Reliability {
    pub residual: R99ErrorRatio,
    pub sdu: Option<R99ErrorRatio>,
    pub delivery: R99ErrorDelivery,
}
impl R99Reliability {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.residual).validate_schema()?;
        if let Some(value) = &self.sdu {
            (value).validate_schema()?;
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99ServiceContract {
    pub traffic_class: R99TrafficClass,
    pub delivery_order: bool,
    pub maximum_transfer_delay_ms: Option<u16>,
    pub subflows: Vec<R99Reliability>,
}
impl R99ServiceContract {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if let Some(value) = &self.maximum_transfer_delay_ms {
            if !(1..=65535).contains(value) {
                anyhow::bail!("R99ServiceContract.maximum_transfer_delay_ms outside 1..=65535");
            }
        }
        for value in &self.subflows {
            (value).validate_schema()?;
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum R99CodingKind {
    #[serde(rename = "convolutional-half")]
    ConvolutionalHalf,
    #[default]
    #[serde(rename = "convolutional-third")]
    ConvolutionalThird,
    #[serde(rename = "turbo")]
    Turbo,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99Coding {
    #[serde(default)]
    pub kind: R99CodingKind,
    #[serde(default = "default_r99_planner_crc_bits")]
    pub crc_bits: u8,
    #[serde(default = "default_r99_planner_rate_matching_attribute")]
    pub rate_matching_attribute: u16,
}
impl Default for R99Coding {
    fn default() -> Self {
        Self {
            kind: Default::default(),
            crc_bits: default_r99_planner_crc_bits(),
            rate_matching_attribute: default_r99_planner_rate_matching_attribute(),
        }
    }
}
impl R99Coding {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(0..=24).contains(&self.crc_bits) {
            anyhow::bail!("R99Coding.crc_bits outside 0..=24");
        }
        if !(1..=256).contains(&self.rate_matching_attribute) {
            anyhow::bail!("R99Coding.rate_matching_attribute outside 1..=256");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99CircuitSubflow {
    pub uplink: R99Coding,
    pub downlink: R99Coding,
    pub empty_block: bool,
}
impl R99CircuitSubflow {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.uplink).validate_schema()?;
        (self.downlink).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99CircuitFormat {
    pub subflow_bits: Vec<u16>,
}
impl R99CircuitFormat {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99CircuitService {
    pub name: String,
    pub contract: R99ServiceContract,
    pub interval_ms: u8,
    pub subflows: Vec<R99CircuitSubflow>,
    pub formats: Vec<R99CircuitFormat>,
    pub versions: u16,
}
impl R99CircuitService {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.contract).validate_schema()?;
        if !(10..=80).contains(&self.interval_ms) {
            anyhow::bail!("R99CircuitService.interval_ms outside 10..=80");
        }
        for value in &self.subflows {
            (value).validate_schema()?;
        }
        for value in &self.formats {
            (value).validate_schema()?;
        }
        if !(1..=65535).contains(&self.versions) {
            anyhow::bail!("R99CircuitService.versions outside 1..=65535");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99PacketChannel {
    pub interval_ms: u8,
    pub rlc_bits: u16,
    pub payload_budget_bits: u16,
    pub maximum_blocks: u16,
    pub coding: R99Coding,
}
impl R99PacketChannel {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(10..=80).contains(&self.interval_ms) {
            anyhow::bail!("R99PacketChannel.interval_ms outside 10..=80");
        }
        if !(16..=5000).contains(&self.rlc_bits) {
            anyhow::bail!("R99PacketChannel.rlc_bits outside 16..=5000");
        }
        if !(1..=5000).contains(&self.payload_budget_bits) {
            anyhow::bail!("R99PacketChannel.payload_budget_bits outside 1..=5000");
        }
        if !(0..=31).contains(&self.maximum_blocks) {
            anyhow::bail!("R99PacketChannel.maximum_blocks outside 0..=31");
        }
        (self.coding).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99BufferLimits {
    pub maximum_sdu_bytes: u32,
    pub queued_sdus: u16,
    pub maximum_buffer_bytes: u32,
}
impl R99BufferLimits {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(1..=4294967295).contains(&self.maximum_sdu_bytes) {
            anyhow::bail!("R99BufferLimits.maximum_sdu_bytes outside 1..=4294967295");
        }
        if !(1..=65535).contains(&self.queued_sdus) {
            anyhow::bail!("R99BufferLimits.queued_sdus outside 1..=65535");
        }
        if !(1..=4294967295).contains(&self.maximum_buffer_bytes) {
            anyhow::bail!("R99BufferLimits.maximum_buffer_bytes outside 1..=4294967295");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99AmPeer {
    pub uplink_window: u16,
    pub downlink_window: u16,
    pub maximum_data_attempts: u16,
    pub maximum_reset_attempts: u16,
    pub reset_timeout_ms: u16,
    pub poll_timeout_ms: Option<u16>,
    pub poll_prohibit_ms: Option<u16>,
    pub poll_every_pdus: Option<u16>,
    pub last_transmission: bool,
    pub last_retransmission: bool,
    pub status_prohibit_ms: Option<u16>,
    pub missing_pdu_indicator: bool,
    pub in_sequence_delivery: bool,
}
impl R99AmPeer {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(1..=4095).contains(&self.uplink_window) {
            anyhow::bail!("R99AmPeer.uplink_window outside 1..=4095");
        }
        if !(1..=4095).contains(&self.downlink_window) {
            anyhow::bail!("R99AmPeer.downlink_window outside 1..=4095");
        }
        if !(1..=65535).contains(&self.maximum_data_attempts) {
            anyhow::bail!("R99AmPeer.maximum_data_attempts outside 1..=65535");
        }
        if !(1..=65535).contains(&self.maximum_reset_attempts) {
            anyhow::bail!("R99AmPeer.maximum_reset_attempts outside 1..=65535");
        }
        if !(1..=65535).contains(&self.reset_timeout_ms) {
            anyhow::bail!("R99AmPeer.reset_timeout_ms outside 1..=65535");
        }
        if let Some(value) = &self.poll_timeout_ms {
            if !(1..=65535).contains(value) {
                anyhow::bail!("R99AmPeer.poll_timeout_ms outside 1..=65535");
            }
        }
        if let Some(value) = &self.poll_prohibit_ms {
            if !(0..=65535).contains(value) {
                anyhow::bail!("R99AmPeer.poll_prohibit_ms outside 0..=65535");
            }
        }
        if let Some(value) = &self.poll_every_pdus {
            if !(1..=65535).contains(value) {
                anyhow::bail!("R99AmPeer.poll_every_pdus outside 1..=65535");
            }
        }
        if let Some(value) = &self.status_prohibit_ms {
            if !(0..=65535).contains(value) {
                anyhow::bail!("R99AmPeer.status_prohibit_ms outside 0..=65535");
            }
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99NetworkAmPolicy {
    pub maximum_data_attempts: u16,
    pub maximum_reset_attempts: u16,
    pub reset_timeout_ms: u16,
    pub poll_timeout_ms: Option<u16>,
    pub poll_prohibit_ms: Option<u16>,
    pub poll_every_pdus: Option<u16>,
    pub last_transmission: bool,
    pub last_retransmission: bool,
    pub status_prohibit_ms: Option<u16>,
    pub missing_pdu_indicator: bool,
}
impl R99NetworkAmPolicy {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(1..=65535).contains(&self.maximum_data_attempts) {
            anyhow::bail!("R99NetworkAmPolicy.maximum_data_attempts outside 1..=65535");
        }
        if !(1..=65535).contains(&self.maximum_reset_attempts) {
            anyhow::bail!("R99NetworkAmPolicy.maximum_reset_attempts outside 1..=65535");
        }
        if !(1..=65535).contains(&self.reset_timeout_ms) {
            anyhow::bail!("R99NetworkAmPolicy.reset_timeout_ms outside 1..=65535");
        }
        if let Some(value) = &self.poll_timeout_ms {
            if !(1..=65535).contains(value) {
                anyhow::bail!("R99NetworkAmPolicy.poll_timeout_ms outside 1..=65535");
            }
        }
        if let Some(value) = &self.poll_prohibit_ms {
            if !(0..=65535).contains(value) {
                anyhow::bail!("R99NetworkAmPolicy.poll_prohibit_ms outside 0..=65535");
            }
        }
        if let Some(value) = &self.poll_every_pdus {
            if !(1..=65535).contains(value) {
                anyhow::bail!("R99NetworkAmPolicy.poll_every_pdus outside 1..=65535");
            }
        }
        if let Some(value) = &self.status_prohibit_ms {
            if !(0..=65535).contains(value) {
                anyhow::bail!("R99NetworkAmPolicy.status_prohibit_ms outside 0..=65535");
            }
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99AmPolicy {
    pub ue: R99AmPeer,
    pub network: R99NetworkAmPolicy,
    pub buffers: R99BufferLimits,
}
impl R99AmPolicy {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.ue).validate_schema()?;
        (self.network).validate_schema()?;
        (self.buffers).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99UmPolicy {
    pub uplink: R99BufferLimits,
    pub downlink: R99BufferLimits,
}
impl R99UmPolicy {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.uplink).validate_schema()?;
        (self.downlink).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99PacketService {
    pub name: String,
    pub contract: R99ServiceContract,
    pub uplink: R99PacketChannel,
    pub downlink: R99PacketChannel,
    #[serde(default)]
    pub am: Option<R99AmPolicy>,
    #[serde(default)]
    pub um: Option<R99UmPolicy>,
}
impl R99PacketService {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.contract).validate_schema()?;
        (self.uplink).validate_schema()?;
        (self.downlink).validate_schema()?;
        if let Some(value) = &self.am {
            (value).validate_schema()?;
        }
        if let Some(value) = &self.um {
            (value).validate_schema()?;
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99ServicesConfig {
    #[serde(default = "default_r99_circuit_services")]
    pub circuit: Vec<R99CircuitService>,
    #[serde(default = "default_r99_packet_services")]
    pub packet: Vec<R99PacketService>,
}
impl Default for R99ServicesConfig {
    fn default() -> Self {
        Self {
            circuit: default_r99_circuit_services(),
            packet: default_r99_packet_services(),
        }
    }
}
impl R99ServicesConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        for value in &self.circuit {
            (value).validate_schema()?;
        }
        for value in &self.packet {
            (value).validate_schema()?;
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct SecondaryCarrierSettings {
    #[serde(default)]
    pub enabled: bool,
    #[doc = "Required when the secondary carrier is enabled."]
    #[serde(default)]
    pub uarfcn_dl: Option<u32>,
    #[doc = "Defaults to the primary carrier scrambling code plus one."]
    #[serde(default)]
    pub primary_scrambling_code: Option<u16>,
    #[doc = "Percentage of the composite power allocated to the secondary carrier."]
    #[serde(default = "default_secondary_carrier_power_percent")]
    pub power_percent: f64,
}
impl Default for SecondaryCarrierSettings {
    fn default() -> Self {
        Self {
            enabled: Default::default(),
            uarfcn_dl: Default::default(),
            primary_scrambling_code: Default::default(),
            power_percent: default_secondary_carrier_power_percent(),
        }
    }
}
impl SecondaryCarrierSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(self.power_percent).is_finite() {
            anyhow::bail!("SecondaryCarrierSettings.power_percent must be finite");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct CellPhysicalSettings {
    #[serde(default = "default_downlink_scrambling_code")]
    pub downlink_scrambling_code: u16,
    #[serde(default)]
    pub dch_secondary_scrambling_code: u8,
}
impl Default for CellPhysicalSettings {
    fn default() -> Self {
        Self {
            downlink_scrambling_code: default_downlink_scrambling_code(),
            dch_secondary_scrambling_code: Default::default(),
        }
    }
}
impl CellPhysicalSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PrachConfig {
    #[doc = "TS 25.331 10.3.6.11 PRACH open-loop power constant, dB. Lower values reduce initial access power without changing dedicated-channel SIR targets."]
    #[serde(default = "default_prach_initial_power_constant_db")]
    pub initial_power_constant_db: i8,
    #[doc = "One advertised/searchable preamble signature (0..15)."]
    #[serde(default = "default_prach_signature")]
    pub signature: u8,
    #[doc = "One advertised/searchable PRACH subchannel (0..11)."]
    #[serde(default = "default_prach_subchannel")]
    pub subchannel: u8,
    #[doc = "TS25.331 preamble scrambling word number, relative to the primary code group (0..15)."]
    #[serde(default = "default_prach_scrambling_code_word_number")]
    pub scrambling_code_word_number: u8,
    #[doc = "TS25.331 DynamicPersistenceLevel (1..8)."]
    #[serde(default = "default_prach_dynamic_persistence_level")]
    pub dynamic_persistence_level: u8,
}
impl Default for PrachConfig {
    fn default() -> Self {
        Self {
            initial_power_constant_db: default_prach_initial_power_constant_db(),
            signature: default_prach_signature(),
            subchannel: default_prach_subchannel(),
            scrambling_code_word_number: default_prach_scrambling_code_word_number(),
            dynamic_persistence_level: default_prach_dynamic_persistence_level(),
        }
    }
}
impl PrachConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(-35..=-10).contains(&self.initial_power_constant_db) {
            anyhow::bail!("PrachConfig.initial_power_constant_db outside -35..=-10");
        }
        if !(0..=15).contains(&self.signature) {
            anyhow::bail!("PrachConfig.signature outside 0..=15");
        }
        if !(0..=11).contains(&self.subchannel) {
            anyhow::bail!("PrachConfig.subchannel outside 0..=11");
        }
        if !(0..=15).contains(&self.scrambling_code_word_number) {
            anyhow::bail!("PrachConfig.scrambling_code_word_number outside 0..=15");
        }
        if !(1..=8).contains(&self.dynamic_persistence_level) {
            anyhow::bail!("PrachConfig.dynamic_persistence_level outside 1..=8");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct CellAccessSettings {
    #[serde(default)]
    pub prach: PrachConfig,
}
impl CellAccessSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.prach).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct CommonRadioConfig {
    #[doc = "AICH waveform offset from the primary pilot, dB; TS 25.331 AICH-PowerOffset (-22..5)."]
    #[serde(default = "default_common_aich_power_offset_db")]
    pub aich_power_offset_db: i8,
}
impl Default for CommonRadioConfig {
    fn default() -> Self {
        Self {
            aich_power_offset_db: default_common_aich_power_offset_db(),
        }
    }
}
impl CommonRadioConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(-22..=5).contains(&self.aich_power_offset_db) {
            anyhow::bail!("CommonRadioConfig.aich_power_offset_db outside -22..=5");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct CellMeasurementsSettings {
    #[serde(default)]
    pub request_cpich_measurements: bool,
    #[serde(default = "default_ue_measurement_report_count")]
    pub ue_measurement_report_count: Option<u8>,
    #[serde(default = "default_ue_measurement_interval_ms")]
    pub ue_measurement_interval_ms: u32,
}
impl Default for CellMeasurementsSettings {
    fn default() -> Self {
        Self {
            request_cpich_measurements: Default::default(),
            ue_measurement_report_count: default_ue_measurement_report_count(),
            ue_measurement_interval_ms: default_ue_measurement_interval_ms(),
        }
    }
}
impl CellMeasurementsSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct CellAdvertisedPowerSettings {
    #[serde(default)]
    pub cpich_tx_dbm: Option<i8>,
    #[serde(default)]
    pub uplink_interference_dbm: Option<i16>,
}
impl CellAdvertisedPowerSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct CellSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_modes: Option<Vec<PacketMode>>,
    #[serde(default)]
    pub secondary_carrier: SecondaryCarrierSettings,
    pub plmn: String,
    pub lac: u16,
    pub rac: u8,
    pub sac: u16,
    pub cell_id: u32,
    pub hnb_identity: String,
    #[serde(default = "default_gprs_enable")]
    pub gprs_enable: bool,
    #[serde(default = "default_nmo")]
    pub nmo: NetworkOperationMode,
    #[serde(default = "default_q_qual_min")]
    pub q_qual_min: i8,
    #[serde(default)]
    pub physical: CellPhysicalSettings,
    #[serde(default)]
    pub access: CellAccessSettings,
    #[serde(default)]
    pub common_channels: CommonRadioConfig,
    #[serde(default)]
    pub measurements: CellMeasurementsSettings,
    #[serde(
        default = "default_broadcast_state_file",
        deserialize_with = "crate::paths::deserialize"
    )]
    pub broadcast_state_file: PathBuf,
    #[serde(default)]
    pub advertised_power: CellAdvertisedPowerSettings,
}
impl CellSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.secondary_carrier).validate_schema()?;
        if !(-24..=0).contains(&self.q_qual_min) {
            anyhow::bail!("CellSettings.q_qual_min outside -24..=0");
        }
        (self.physical).validate_schema()?;
        (self.access).validate_schema()?;
        (self.common_channels).validate_schema()?;
        (self.measurements).validate_schema()?;
        (self.advertised_power).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HnbgwTransport {
    #[default]
    #[serde(rename = "sctp")]
    Sctp,
    #[serde(rename = "usrsctp")]
    Usrsctp,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct HnbgwConfig {
    pub address: String,
    #[serde(default = "default_hnbgw_port")]
    pub port: u16,
    #[doc = "Iuh signaling transport: sctp selects Linux kernel SCTP (default), usrsctp selects raw userspace SCTP."]
    #[serde(default)]
    pub transport: HnbgwTransport,
    #[doc = "HNB-GW UDP port for SCTP over UDP (RFC 6951). Requires transport = usrsctp. Absent selects raw SCTP."]
    #[serde(default)]
    pub udp_encapsulation_port: Option<u16>,
    #[doc = "Local UDP port for SCTP over UDP. Absent selects a free port."]
    #[serde(default)]
    pub udp_encapsulation_local_port: Option<u16>,
    #[doc = "Local IP address for the Iuh association, the only address offered to the HNB-GW.\nAbsent lets kernel SCTP offer every local address."]
    #[serde(default)]
    pub local_address: Option<String>,
}
impl HnbgwConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct GtpuConfig {
    #[doc = "Local IPv4 address for the HNB GTP-U socket."]
    pub bind_ip: String,
    #[doc = "Local UDP port for the HNB GTP-U socket."]
    #[serde(default = "default_gtpu_bind_port")]
    pub bind_port: u16,
    #[doc = "IPv4 address advertised to the SGSN in PS RAB-AssignmentResponse.\nDefaults to bind_ip, but must not resolve to 0.0.0.0."]
    #[serde(default)]
    pub advertise_ip: Option<String>,
    #[doc = "Fall back to an ephemeral GTP-U port if `bind_port` is taken.\nDefault false. (The engine always allows the fallback on the ABI-only\ntest path that runs without a config blob.)"]
    #[serde(default)]
    pub allow_ephemeral_port: bool,
    #[doc = "Request release of the UE PS Iu connection when the GTP-U peer reports a missing tunnel. Default true."]
    #[serde(default = "default_release_rab_on_error_indication")]
    pub release_rab_on_error_indication: bool,
}
impl GtpuConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct IuhConfig {
    #[doc = "Consecutive HNB-GW connection or registration failures to retry before stopping. Zero retries indefinitely."]
    #[serde(default = "default_retry_attempts")]
    pub retry_attempts: u32,
    #[doc = "Delay in seconds between HNB-GW connection attempts."]
    #[serde(default = "default_retry_backoff_secs")]
    pub retry_backoff_secs: u64,
}
impl Default for IuhConfig {
    fn default() -> Self {
        Self {
            retry_attempts: default_retry_attempts(),
            retry_backoff_secs: default_retry_backoff_secs(),
        }
    }
}
impl IuhConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct CoreSettings {
    pub hnbgw: HnbgwConfig,
    pub gtpu: GtpuConfig,
    #[serde(default)]
    pub iuh: IuhConfig,
    #[serde(default)]
    pub hnbap_tmsi_registration: bool,
}
impl CoreSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.hnbgw).validate_schema()?;
        (self.gtpu).validate_schema()?;
        (self.iuh).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99SignallingProfile {
    #[serde(default)]
    pub uplink: R99Coding,
    #[serde(default)]
    pub downlink: R99Coding,
    #[doc = "Transport interval in milliseconds."]
    #[serde(default = "default_r99_planner_interval_ms")]
    pub interval_ms: u8,
    #[serde(default = "default_r99_planner_maximum_blocks")]
    pub maximum_blocks: u16,
    #[doc = "SRB1 uplink RLC PDU size in bits."]
    #[serde(default = "default_r99_planner_um_uplink_bits")]
    pub um_uplink_bits: u16,
    #[doc = "SRB1 downlink RLC PDU size in bits."]
    #[serde(default = "default_r99_planner_um_downlink_bits")]
    pub um_downlink_bits: u16,
}
impl Default for R99SignallingProfile {
    fn default() -> Self {
        Self {
            uplink: Default::default(),
            downlink: Default::default(),
            interval_ms: default_r99_planner_interval_ms(),
            maximum_blocks: default_r99_planner_maximum_blocks(),
            um_uplink_bits: default_r99_planner_um_uplink_bits(),
            um_downlink_bits: default_r99_planner_um_downlink_bits(),
        }
    }
}
impl R99SignallingProfile {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.uplink).validate_schema()?;
        (self.downlink).validate_schema()?;
        if !(10..=80).contains(&self.interval_ms) {
            anyhow::bail!("R99SignallingProfile.interval_ms outside 10..=80");
        }
        if ((self.interval_ms as i128) - 10_i128).rem_euclid(10_i128) != 0 {
            anyhow::bail!("R99SignallingProfile.interval_ms must advance in steps of 10 from 10");
        }
        if !(1..=31).contains(&self.maximum_blocks) {
            anyhow::bail!("R99SignallingProfile.maximum_blocks outside 1..=31");
        }
        if !(16..=5000).contains(&self.um_uplink_bits) {
            anyhow::bail!("R99SignallingProfile.um_uplink_bits outside 16..=5000");
        }
        if ((self.um_uplink_bits as i128) - 16_i128).rem_euclid(8_i128) != 0 {
            anyhow::bail!("R99SignallingProfile.um_uplink_bits must advance in steps of 8 from 16");
        }
        if !(24..=5000).contains(&self.um_downlink_bits) {
            anyhow::bail!("R99SignallingProfile.um_downlink_bits outside 24..=5000");
        }
        if ((self.um_downlink_bits as i128) - 24_i128).rem_euclid(8_i128) != 0 {
            anyhow::bail!(
                "R99SignallingProfile.um_downlink_bits must advance in steps of 8 from 24"
            );
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum R99TpcAlgorithm {
    #[default]
    #[serde(rename = "algorithm-1")]
    Algorithm1,
    #[serde(rename = "algorithm-2")]
    Algorithm2,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99PhysicalCandidate {
    #[serde(default = "default_r99_planner_uplink_minimum_sf")]
    pub uplink_minimum_sf: u16,
    #[serde(default = "default_r99_planner_uplink_dpdch_count")]
    pub uplink_dpdch_count: u8,
    #[serde(default = "default_r99_planner_tfci")]
    pub uplink_tfci: bool,
    #[doc = "DPCCH power offset in dB."]
    #[serde(default = "default_r99_planner_dpcch_offset_db")]
    pub dpcch_offset_db: i16,
    #[serde(default = "default_r99_planner_frame_delay")]
    pub pc_preamble_frames: u8,
    #[serde(default = "default_r99_planner_frame_delay")]
    pub srb_delay_frames: u8,
    #[serde(default)]
    pub tpc_algorithm: R99TpcAlgorithm,
    #[serde(default = "default_r99_planner_tpc_step_db")]
    pub tpc_step_db: u8,
    #[serde(default = "default_r99_planner_downlink_sf")]
    pub downlink_sf: u16,
    #[serde(default = "default_r99_planner_downlink_pilot_bits")]
    pub downlink_pilot_bits: u8,
    #[serde(default = "default_r99_planner_tfci")]
    pub downlink_tfci: bool,
    #[serde(default = "default_r99_planner_downlink_code_count")]
    pub downlink_code_count: u8,
    #[serde(default = "default_r99_planner_flexible_position")]
    pub flexible_position: bool,
    #[doc = "Downlink pilot offset in quarter-dB units."]
    #[serde(default = "default_r99_planner_pilot_offset_quarter_db")]
    pub pilot_offset_quarter_db: u8,
    #[doc = "Default DPCH timing offset in 512-chip units."]
    #[serde(default = "default_r99_planner_offset_512_chips")]
    pub default_offset_512_chips: u16,
}
impl Default for R99PhysicalCandidate {
    fn default() -> Self {
        Self {
            uplink_minimum_sf: default_r99_planner_uplink_minimum_sf(),
            uplink_dpdch_count: default_r99_planner_uplink_dpdch_count(),
            uplink_tfci: default_r99_planner_tfci(),
            dpcch_offset_db: default_r99_planner_dpcch_offset_db(),
            pc_preamble_frames: default_r99_planner_frame_delay(),
            srb_delay_frames: default_r99_planner_frame_delay(),
            tpc_algorithm: Default::default(),
            tpc_step_db: default_r99_planner_tpc_step_db(),
            downlink_sf: default_r99_planner_downlink_sf(),
            downlink_pilot_bits: default_r99_planner_downlink_pilot_bits(),
            downlink_tfci: default_r99_planner_tfci(),
            downlink_code_count: default_r99_planner_downlink_code_count(),
            flexible_position: default_r99_planner_flexible_position(),
            pilot_offset_quarter_db: default_r99_planner_pilot_offset_quarter_db(),
            default_offset_512_chips: default_r99_planner_offset_512_chips(),
        }
    }
}
impl R99PhysicalCandidate {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(4..=256).contains(&self.uplink_minimum_sf) {
            anyhow::bail!("R99PhysicalCandidate.uplink_minimum_sf outside 4..=256");
        }
        if !(1..=6).contains(&self.uplink_dpdch_count) {
            anyhow::bail!("R99PhysicalCandidate.uplink_dpdch_count outside 1..=6");
        }
        if !(-164..=-6).contains(&self.dpcch_offset_db) {
            anyhow::bail!("R99PhysicalCandidate.dpcch_offset_db outside -164..=-6");
        }
        if ((self.dpcch_offset_db as i128) - -164_i128).rem_euclid(2_i128) != 0 {
            anyhow::bail!(
                "R99PhysicalCandidate.dpcch_offset_db must advance in steps of 2 from -164"
            );
        }
        if !(0..=7).contains(&self.pc_preamble_frames) {
            anyhow::bail!("R99PhysicalCandidate.pc_preamble_frames outside 0..=7");
        }
        if !(0..=7).contains(&self.srb_delay_frames) {
            anyhow::bail!("R99PhysicalCandidate.srb_delay_frames outside 0..=7");
        }
        if !(1..=2).contains(&self.tpc_step_db) {
            anyhow::bail!("R99PhysicalCandidate.tpc_step_db outside 1..=2");
        }
        if !(4..=512).contains(&self.downlink_sf) {
            anyhow::bail!("R99PhysicalCandidate.downlink_sf outside 4..=512");
        }
        if !(2..=16).contains(&self.downlink_pilot_bits) {
            anyhow::bail!("R99PhysicalCandidate.downlink_pilot_bits outside 2..=16");
        }
        if !(1..=8).contains(&self.downlink_code_count) {
            anyhow::bail!("R99PhysicalCandidate.downlink_code_count outside 1..=8");
        }
        if !(0..=24).contains(&self.pilot_offset_quarter_db) {
            anyhow::bail!("R99PhysicalCandidate.pilot_offset_quarter_db outside 0..=24");
        }
        if !(0..=599).contains(&self.default_offset_512_chips) {
            anyhow::bail!("R99PhysicalCandidate.default_offset_512_chips outside 0..=599");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99PlannerBudgets {
    #[serde(default = "default_r99_planner_maximum_combinations")]
    pub maximum_combinations: u16,
    #[serde(default = "default_r99_planner_maximum_total_coded_bits")]
    pub maximum_total_coded_bits: u32,
    #[serde(default = "default_r99_planner_maximum_user_storage_bytes")]
    pub maximum_user_storage_bytes: u32,
    #[serde(default = "default_r99_planner_maximum_transport_coded_bits")]
    pub maximum_transport_coded_bits: u32,
    #[serde(default = "default_r99_planner_maximum_mapping_entries")]
    pub maximum_mapping_entries: u32,
}
impl Default for R99PlannerBudgets {
    fn default() -> Self {
        Self {
            maximum_combinations: default_r99_planner_maximum_combinations(),
            maximum_total_coded_bits: default_r99_planner_maximum_total_coded_bits(),
            maximum_user_storage_bytes: default_r99_planner_maximum_user_storage_bytes(),
            maximum_transport_coded_bits: default_r99_planner_maximum_transport_coded_bits(),
            maximum_mapping_entries: default_r99_planner_maximum_mapping_entries(),
        }
    }
}
impl R99PlannerBudgets {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(1..=1024).contains(&self.maximum_combinations) {
            anyhow::bail!("R99PlannerBudgets.maximum_combinations outside 1..=1024");
        }
        if !(1..=4294967295).contains(&self.maximum_total_coded_bits) {
            anyhow::bail!("R99PlannerBudgets.maximum_total_coded_bits outside 1..=4294967295");
        }
        if !(1..=4294967295).contains(&self.maximum_user_storage_bytes) {
            anyhow::bail!("R99PlannerBudgets.maximum_user_storage_bytes outside 1..=4294967295");
        }
        if !(1..=4294967295).contains(&self.maximum_transport_coded_bits) {
            anyhow::bail!("R99PlannerBudgets.maximum_transport_coded_bits outside 1..=4294967295");
        }
        if !(1..=4294967295).contains(&self.maximum_mapping_entries) {
            anyhow::bail!("R99PlannerBudgets.maximum_mapping_entries outside 1..=4294967295");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99PlannerConfig {
    #[doc = "Declared Rel99 services. The defaults are not validated over the air."]
    #[serde(default)]
    pub services: R99ServicesConfig,
    #[serde(default)]
    pub signalling: R99SignallingProfile,
    #[serde(default = "default_r99_planner_physical_candidates")]
    pub physical_candidates: Vec<R99PhysicalCandidate>,
    #[serde(default)]
    pub budgets: R99PlannerBudgets,
    #[serde(default = "default_r99_planner_uplink_gain")]
    pub uplink_control_gain: u8,
    #[serde(default = "default_r99_planner_uplink_gain")]
    pub uplink_data_gain: u8,
}
impl Default for R99PlannerConfig {
    fn default() -> Self {
        Self {
            services: Default::default(),
            signalling: Default::default(),
            physical_candidates: default_r99_planner_physical_candidates(),
            budgets: Default::default(),
            uplink_control_gain: default_r99_planner_uplink_gain(),
            uplink_data_gain: default_r99_planner_uplink_gain(),
        }
    }
}
impl R99PlannerConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.services).validate_schema()?;
        (self.signalling).validate_schema()?;
        for value in &self.physical_candidates {
            (value).validate_schema()?;
        }
        (self.budgets).validate_schema()?;
        if !(1..=15).contains(&self.uplink_control_gain) {
            anyhow::bail!("R99PlannerConfig.uplink_control_gain outside 1..=15");
        }
        if !(1..=15).contains(&self.uplink_data_gain) {
            anyhow::bail!("R99PlannerConfig.uplink_data_gain outside 1..=15");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct R99RadioConfig {
    #[doc = "Rel99 planner policy. The compiled defaults are not validated over the air."]
    #[serde(default)]
    pub planner: R99PlannerConfig,
    #[doc = "Initial puncturing limit: TS 25.331 PuncturingLimit enumerates 40..100 percent in steps of 4; transport profiles set their derived limits."]
    #[serde(default = "default_r99_initial_ul_puncturing_limit_percent")]
    pub initial_ul_puncturing_limit_percent: u8,
    #[doc = "Base for per-UE long-code allocation; actual assigned codes are runtime state (24 bits)."]
    #[serde(default = "default_r99_uplink_scrambling_code_base")]
    pub uplink_scrambling_code_base: u32,
}
impl Default for R99RadioConfig {
    fn default() -> Self {
        Self {
            planner: Default::default(),
            initial_ul_puncturing_limit_percent: default_r99_initial_ul_puncturing_limit_percent(),
            uplink_scrambling_code_base: default_r99_uplink_scrambling_code_base(),
        }
    }
}
impl R99RadioConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.planner).validate_schema()?;
        if !(40..=100).contains(&self.initial_ul_puncturing_limit_percent) {
            anyhow::bail!("R99RadioConfig.initial_ul_puncturing_limit_percent outside 40..=100");
        }
        if ((self.initial_ul_puncturing_limit_percent as i128) - 40_i128).rem_euclid(4_i128) != 0 {
            anyhow::bail!(
                "R99RadioConfig.initial_ul_puncturing_limit_percent must advance in steps of 4 from 40"
            );
        }
        if !(0..=16777215).contains(&self.uplink_scrambling_code_base) {
            anyhow::bail!("R99RadioConfig.uplink_scrambling_code_base outside 0..=16777215");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Rel99Settings {
    #[serde(default)]
    pub profile: Option<crate::Rel99Profile>,
    #[serde(default)]
    pub physical: R99RadioConfig,
}

impl Rel99Settings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.physical).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RrcReleaseTransportPolicy {
    #[default]
    #[serde(rename = "auto")]
    Automatic,
    #[serde(rename = "replace")]
    ReplaceTfcs,
    #[serde(rename = "retain")]
    Retain,
    #[serde(rename = "reaffirm")]
    ReaffirmTfcs,
    #[serde(rename = "remove")]
    RemoveTransports,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RrcConfig {
    #[doc = "Bearer-release transport construction policy. auto selects the standards-based construction."]
    #[serde(default)]
    pub release_transport_policy: RrcReleaseTransportPolicy,
    #[doc = "Initial serving RNC identity (12 bits)."]
    #[serde(default = "default_rrc_srnc_id")]
    pub srnc_id: u16,
    #[doc = "URA identity advertised in RRC, not a PLMN identity."]
    #[serde(default = "default_rrc_ura_identity")]
    pub ura_identity: u16,
    #[doc = "TS25.331 CN-DRX-CycleLengthCoefficient (6..9)."]
    #[serde(default = "default_rrc_cn_drx_cycle_length_coefficient")]
    pub cn_drx_cycle_length_coefficient: u8,
    #[doc = "Encoded CS domain periodic LU timer octet; retained value30."]
    #[serde(default = "default_rrc_periodic_lu_timer_value")]
    pub periodic_lu_timer_value: u8,
    #[doc = "TS25.331 MaxAllowedUL-TX-Power, dBm (-50..33)."]
    #[serde(default = "default_rrc_max_allowed_ul_tx_power_dbm")]
    pub max_allowed_ul_tx_power_dbm: i8,
    #[doc = "Seconds without NAS or user-packet activity before requesting Iu and RRC release. Zero disables inactivity release. Active calls, pending procedures and queued data are protected."]
    #[serde(default = "default_rrc_inactivity_release_seconds")]
    pub inactivity_release_seconds: u32,
    #[doc = "N308: maximum UE RRC Connection Release Complete retransmissions in CELL_DCH."]
    #[serde(default = "default_rrc_release_complete_retransmissions")]
    pub release_complete_retransmissions: u8,
    #[doc = "Total idle seconds before reclaiming a context with an unanswered RRC release. Zero disables this timeout, not cleanup of a completed release. Must be at least the release threshold. Never overrides an open Iu connection or pending RRC transmission."]
    #[serde(default = "default_rrc_inactivity_delete_seconds")]
    pub inactivity_delete_seconds: u32,
    #[doc = "Allow joint bearer construction; radio.cs_ps_combined_data separately controls in-call data policy."]
    #[serde(default = "default_rrc_combined_bearer_setup")]
    pub combined_bearer_setup: bool,
}
impl Default for RrcConfig {
    fn default() -> Self {
        Self {
            release_transport_policy: Default::default(),
            srnc_id: default_rrc_srnc_id(),
            ura_identity: default_rrc_ura_identity(),
            cn_drx_cycle_length_coefficient: default_rrc_cn_drx_cycle_length_coefficient(),
            periodic_lu_timer_value: default_rrc_periodic_lu_timer_value(),
            max_allowed_ul_tx_power_dbm: default_rrc_max_allowed_ul_tx_power_dbm(),
            inactivity_release_seconds: default_rrc_inactivity_release_seconds(),
            release_complete_retransmissions: default_rrc_release_complete_retransmissions(),
            inactivity_delete_seconds: default_rrc_inactivity_delete_seconds(),
            combined_bearer_setup: default_rrc_combined_bearer_setup(),
        }
    }
}
impl RrcConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(0..=4095).contains(&self.srnc_id) {
            anyhow::bail!("RrcConfig.srnc_id outside 0..=4095");
        }
        if !(6..=9).contains(&self.cn_drx_cycle_length_coefficient) {
            anyhow::bail!("RrcConfig.cn_drx_cycle_length_coefficient outside 6..=9");
        }
        if !(-50..=33).contains(&self.max_allowed_ul_tx_power_dbm) {
            anyhow::bail!("RrcConfig.max_allowed_ul_tx_power_dbm outside -50..=33");
        }
        if !(0..=2147483).contains(&self.inactivity_release_seconds) {
            anyhow::bail!("RrcConfig.inactivity_release_seconds outside 0..=2147483");
        }
        if !(1..=8).contains(&self.release_complete_retransmissions) {
            anyhow::bail!("RrcConfig.release_complete_retransmissions outside 1..=8");
        }
        if !(0..=2147483).contains(&self.inactivity_delete_seconds) {
            anyhow::bail!("RrcConfig.inactivity_delete_seconds outside 0..=2147483");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    #[doc = "Log filter applied at startup. RUST_LOG overrides this value when set."]
    #[serde(default = "default_log_filter")]
    pub filter: String,
}
impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: default_log_filter(),
        }
    }
}
impl LogConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsSettings {
    #[serde(default)]
    pub logging: LogConfig,
}
impl DiagnosticsSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.logging).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ManagementConfig {
    #[doc = "Enables the management server."]
    #[serde(default = "default_management_enabled")]
    pub enabled: bool,
    #[doc = "Read-only gRPC Unix socket."]
    #[serde(
        default = "default_management_socket",
        deserialize_with = "crate::paths::deserialize"
    )]
    pub socket_path: PathBuf,
    #[doc = "Owner-only administrative gRPC Unix socket."]
    #[serde(
        default = "default_management_administrator_socket",
        deserialize_with = "crate::paths::deserialize"
    )]
    pub administrator_socket_path: PathBuf,
    #[doc = "Optional mutual-TLS listen address."]
    #[serde(default)]
    pub tcp_address: Option<String>,
    #[doc = "Server certificate PEM file."]
    #[serde(default)]
    pub tls_certificate: Option<PathBuf>,
    #[doc = "Server private-key PEM file."]
    #[serde(default)]
    pub tls_key: Option<PathBuf>,
    #[doc = "Trusted client-certificate CA PEM file."]
    #[serde(default)]
    pub tls_client_ca: Option<PathBuf>,
    #[doc = "Lowercase SHA256 fingerprints of administrative client leaf certificates."]
    #[serde(default)]
    pub administrator_certificates_sha256: Vec<String>,
    #[doc = "Private root for API-requested bounded I/Q captures."]
    #[serde(
        default = "default_management_capture_directory",
        deserialize_with = "crate::paths::deserialize"
    )]
    pub capture_directory: PathBuf,
    #[doc = "Disk budget for retained management captures. Existing files are never automatically deleted."]
    #[serde(default = "default_management_capture_disk_mib")]
    pub maximum_capture_disk_mib: u32,
}
impl Default for ManagementConfig {
    fn default() -> Self {
        Self {
            enabled: default_management_enabled(),
            socket_path: default_management_socket(),
            administrator_socket_path: default_management_administrator_socket(),
            tcp_address: Default::default(),
            tls_certificate: Default::default(),
            tls_key: Default::default(),
            tls_client_ca: Default::default(),
            administrator_certificates_sha256: Default::default(),
            capture_directory: default_management_capture_directory(),
            maximum_capture_disk_mib: default_management_capture_disk_mib(),
        }
    }
}
impl ManagementConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(1..=65536).contains(&self.maximum_capture_disk_mib) {
            anyhow::bail!("ManagementConfig.maximum_capture_disk_mib outside 1..=65536");
        }
        Ok(())
    }
}

#[derive(Default, Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ManagementSettings {
    #[serde(default)]
    pub grpc: ManagementConfig,
}
impl ManagementSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.grpc).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ThreadSchedulingConfig {
    #[doc = "Apply and verify thread scheduling at startup. Requires Linux real-time permissions. Failure stops startup rather than silently serving without the requested policy."]
    #[serde(default = "default_scheduling_enabled")]
    pub enabled: bool,
    #[doc = "Controller CPU. Null uses the startup allowed CPU mask."]
    #[serde(default)]
    pub main_cpu: Option<u32>,
    #[doc = "Background and native driver I/O CPU. Null uses the startup allowed CPU mask."]
    #[serde(default)]
    pub control_cpu: Option<u32>,
    #[doc = "Named USB event thread CPU. Null uses the startup allowed CPU mask."]
    #[serde(default)]
    pub usb_cpu: Option<u32>,
    #[doc = "TX waveform preparation CPU. Null uses the startup allowed CPU mask."]
    #[serde(default)]
    pub prepare_cpu: Option<u32>,
    #[doc = "SDR submission CPU. Null uses the startup allowed CPU mask."]
    #[serde(default)]
    pub submit_cpu: Option<u32>,
    #[doc = "Receive DSP CPU. Null uses the startup allowed CPU mask."]
    #[serde(default)]
    pub receive_cpu: Option<u32>,
    #[doc = "SDR receive reader CPU. Null uses the startup allowed CPU mask."]
    #[serde(default)]
    pub reader_cpu: Option<u32>,
    #[doc = "FIFO priority for native SDR I/O threads."]
    #[serde(default = "default_usb_priority")]
    pub usb_priority: u8,
    #[doc = "FIFO priority for SDR submission."]
    #[serde(default = "default_io_priority")]
    pub submit_priority: u8,
    #[doc = "FIFO priority for SDR receive reads."]
    #[serde(default = "default_io_priority")]
    pub reader_priority: u8,
    #[doc = "FIFO priority for TX preparation."]
    #[serde(default = "default_prepare_priority")]
    pub prepare_priority: u8,
    #[doc = "FIFO priority for receive DSP."]
    #[serde(default = "default_receive_priority")]
    pub receive_priority: u8,
}
impl Default for ThreadSchedulingConfig {
    fn default() -> Self {
        Self {
            enabled: default_scheduling_enabled(),
            main_cpu: Default::default(),
            control_cpu: Default::default(),
            usb_cpu: Default::default(),
            prepare_cpu: Default::default(),
            submit_cpu: Default::default(),
            receive_cpu: Default::default(),
            reader_cpu: Default::default(),
            usb_priority: default_usb_priority(),
            submit_priority: default_io_priority(),
            reader_priority: default_io_priority(),
            prepare_priority: default_prepare_priority(),
            receive_priority: default_receive_priority(),
        }
    }
}
impl ThreadSchedulingConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(1..=99).contains(&self.usb_priority) {
            anyhow::bail!("ThreadSchedulingConfig.usb_priority outside 1..=99");
        }
        if !(1..=99).contains(&self.submit_priority) {
            anyhow::bail!("ThreadSchedulingConfig.submit_priority outside 1..=99");
        }
        if !(1..=99).contains(&self.reader_priority) {
            anyhow::bail!("ThreadSchedulingConfig.reader_priority outside 1..=99");
        }
        if !(1..=99).contains(&self.prepare_priority) {
            anyhow::bail!("ThreadSchedulingConfig.prepare_priority outside 1..=99");
        }
        if !(1..=99).contains(&self.receive_priority) {
            anyhow::bail!("ThreadSchedulingConfig.receive_priority outside 1..=99");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SimdMode {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "scalar")]
    Scalar,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RustCircuitInitialization {
    #[default]
    #[serde(rename = "initiate")]
    Initiate,
    #[serde(rename = "receive")]
    Receive,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RustCircuitConfig {
    #[doc = "RTP bind address. When absent, inherits gtpu.bind_ip. RTP and consecutive RTCP ports are allocated together."]
    #[serde(default)]
    pub bind_ip: Option<String>,
    #[doc = "Advertised RTP address. When absent, inherits the canonical GTP-U advertised address."]
    #[serde(default)]
    pub advertise_ip: Option<String>,
    #[doc = "RTP payload type for Iu-UP."]
    #[serde(default = "default_circuit_payload_type")]
    pub payload_type: u8,
    #[doc = "Per-route UDP receive buffer including a truncation sentinel byte."]
    #[serde(default = "default_circuit_datagram_bytes")]
    pub datagram_bytes: u16,
    #[doc = "Uplink subflow assembly deadline in milliseconds."]
    #[serde(default = "default_circuit_wait_ms")]
    pub wait_ms: u32,
    #[doc = "Initial downlink playout lead in milliseconds before alignment to the admitted transport period."]
    #[serde(default = "default_circuit_playout_lead_ms")]
    pub playout_lead_ms: u32,
    #[doc = "Maximum RTP timestamp distance from the route's latest accepted forward data packet, in milliseconds. Exceeding it rejects data without retiming."]
    #[serde(default = "default_circuit_horizon_ms")]
    pub horizon_ms: u32,
    #[doc = "Iu-UP initialization role for a new circuit route."]
    #[serde(default)]
    pub initialization: RustCircuitInitialization,
    #[doc = "Implemented Iu-UP mode-version bitmask. Only versions 1 and 2 are supported."]
    #[serde(default = "default_circuit_versions")]
    pub versions: u16,
    #[doc = "O&M-selected TINIT duration in milliseconds, not a specification default."]
    #[serde(default = "default_circuit_timeout_ms")]
    pub timeout_ms: u32,
    #[doc = "O&M-selected NINIT repetition limit, not a specification default."]
    #[serde(default = "default_circuit_repetitions")]
    pub repetitions: u8,
}
impl Default for RustCircuitConfig {
    fn default() -> Self {
        Self {
            bind_ip: Default::default(),
            advertise_ip: Default::default(),
            payload_type: default_circuit_payload_type(),
            datagram_bytes: default_circuit_datagram_bytes(),
            wait_ms: default_circuit_wait_ms(),
            playout_lead_ms: default_circuit_playout_lead_ms(),
            horizon_ms: default_circuit_horizon_ms(),
            initialization: Default::default(),
            versions: default_circuit_versions(),
            timeout_ms: default_circuit_timeout_ms(),
            repetitions: default_circuit_repetitions(),
        }
    }
}
impl RustCircuitConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(0..=127).contains(&self.payload_type) {
            anyhow::bail!("RustCircuitConfig.payload_type outside 0..=127");
        }
        if !(16..=65507).contains(&self.datagram_bytes) {
            anyhow::bail!("RustCircuitConfig.datagram_bytes outside 16..=65507");
        }
        if !(1..=10000).contains(&self.wait_ms) {
            anyhow::bail!("RustCircuitConfig.wait_ms outside 1..=10000");
        }
        if !(1..=10000).contains(&self.playout_lead_ms) {
            anyhow::bail!("RustCircuitConfig.playout_lead_ms outside 1..=10000");
        }
        if !(1..=86400000).contains(&self.horizon_ms) {
            anyhow::bail!("RustCircuitConfig.horizon_ms outside 1..=86400000");
        }
        if !(1..=3).contains(&self.versions) {
            anyhow::bail!("RustCircuitConfig.versions outside 1..=3");
        }
        if !(1..=60000).contains(&self.timeout_ms) {
            anyhow::bail!("RustCircuitConfig.timeout_ms outside 1..=60000");
        }
        if !(0..=10).contains(&self.repetitions) {
            anyhow::bail!("RustCircuitConfig.repetitions outside 0..=10");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RustMediaConfig {
    #[doc = "Cell-wide packet queue, transport storage and UDP scratch budget, independent of signaling queues."]
    #[serde(default = "default_media_packet_bytes")]
    pub maximum_packet_bytes: u32,
    #[doc = "Cell-wide voice media and negotiation storage budget, independent of signaling queues."]
    #[serde(default = "default_media_voice_bytes")]
    pub maximum_voice_bytes: u32,
    #[doc = "Declared circuit transport policy. The defaults are not validated over the air."]
    #[serde(default)]
    pub circuit: RustCircuitConfig,
}
impl Default for RustMediaConfig {
    fn default() -> Self {
        Self {
            maximum_packet_bytes: default_media_packet_bytes(),
            maximum_voice_bytes: default_media_voice_bytes(),
            circuit: Default::default(),
        }
    }
}
impl RustMediaConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(1..=268435456).contains(&self.maximum_packet_bytes) {
            anyhow::bail!("RustMediaConfig.maximum_packet_bytes outside 1..=268435456");
        }
        if !(1..=268435456).contains(&self.maximum_voice_bytes) {
            anyhow::bail!("RustMediaConfig.maximum_voice_bytes outside 1..=268435456");
        }
        (self.circuit).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RustClockSamplingConfig {
    #[doc = "Interval between device-clock samples, in microseconds."]
    #[serde(default = "default_clock_sampling_interval_us")]
    pub interval_us: u32,
    #[doc = "Optional accepted clock-sample age limit in microseconds. Omitted or null retains the last anchor without age expiry. Timestamp consistency, uncertainty and radio deadline checks still apply."]
    #[serde(default = "default_clock_sampling_maximum_age_us")]
    pub maximum_age_us: Option<u32>,
    #[doc = "Device-clock query duration that bounds the clock, in microseconds. Persistently slower queries are placed at their midpoint and assumed accurate to this window."]
    #[serde(default = "default_clock_sampling_maximum_query_us")]
    pub maximum_query_us: u32,
    #[doc = "Assumed maximum host/device frequency difference in parts per million, not RF calibration."]
    #[serde(default = "default_clock_sampling_maximum_drift_ppm")]
    pub maximum_drift_ppm: u32,
}
impl Default for RustClockSamplingConfig {
    fn default() -> Self {
        Self {
            interval_us: default_clock_sampling_interval_us(),
            maximum_age_us: default_clock_sampling_maximum_age_us(),
            maximum_query_us: default_clock_sampling_maximum_query_us(),
            maximum_drift_ppm: default_clock_sampling_maximum_drift_ppm(),
        }
    }
}
impl RustClockSamplingConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(1000..=1000000).contains(&self.interval_us) {
            anyhow::bail!("RustClockSamplingConfig.interval_us outside 1000..=1000000");
        }
        if let Some(value) = &self.maximum_age_us {
            if !(1000..=2000000).contains(value) {
                anyhow::bail!("RustClockSamplingConfig.maximum_age_us outside 1000..=2000000");
            }
        }
        if !(1..=100000).contains(&self.maximum_query_us) {
            anyhow::bail!("RustClockSamplingConfig.maximum_query_us outside 1..=100000");
        }
        if !(1..=10000).contains(&self.maximum_drift_ppm) {
            anyhow::bail!("RustClockSamplingConfig.maximum_drift_ppm outside 1..=10000");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RustServingRecoveryConfig {
    #[doc = "Lifetime per-process restart limit after proven epoch retirement. Zero disables automatic restart. The budget does not reset implicitly."]
    #[serde(default = "default_serving_recovery_maximum_restarts")]
    pub maximum_restarts: u16,
    #[doc = "Initial automatic restart delay in milliseconds. Subsequent delays increase exponentially up to maximum_backoff_ms."]
    #[serde(default = "default_serving_recovery_backoff_ms")]
    pub backoff_ms: u32,
    #[doc = "Maximum automatic restart delay in milliseconds. Must be at least backoff_ms."]
    #[serde(default = "default_serving_recovery_maximum_backoff_ms")]
    pub maximum_backoff_ms: u32,
}
impl Default for RustServingRecoveryConfig {
    fn default() -> Self {
        Self {
            maximum_restarts: default_serving_recovery_maximum_restarts(),
            backoff_ms: default_serving_recovery_backoff_ms(),
            maximum_backoff_ms: default_serving_recovery_maximum_backoff_ms(),
        }
    }
}
impl RustServingRecoveryConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(0..=100).contains(&self.maximum_restarts) {
            anyhow::bail!("RustServingRecoveryConfig.maximum_restarts outside 0..=100");
        }
        if !(1..=60000).contains(&self.backoff_ms) {
            anyhow::bail!("RustServingRecoveryConfig.backoff_ms outside 1..=60000");
        }
        if !(1..=300000).contains(&self.maximum_backoff_ms) {
            anyhow::bail!("RustServingRecoveryConfig.maximum_backoff_ms outside 1..=300000");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSettings {
    #[serde(default)]
    pub dch_cpu: Option<u32>,
    #[serde(default)]
    pub scheduling: ThreadSchedulingConfig,
    #[serde(default)]
    pub simd: SimdMode,
    #[serde(default)]
    pub media: RustMediaConfig,
    #[serde(default)]
    pub clock: RustClockSamplingConfig,
    #[serde(default)]
    pub recovery: RustServingRecoveryConfig,
    #[serde(default)]
    pub spin_wait: bool,
    #[doc = "Lock all current and future process memory at startup so realtime threads never page-fault."]
    #[serde(default)]
    pub lock_memory: bool,
    #[serde(default = "default_serving_tx_batch_slots")]
    pub tx_batch_slots: u8,
    #[serde(default = "default_serving_maximum_ues")]
    pub maximum_ues: u32,
    #[serde(default = "default_serving_maximum_sdu_bytes")]
    pub maximum_sdu_bytes: u32,
    #[serde(default = "default_serving_queued_sdus_per_bearer")]
    pub queued_sdus_per_bearer: u32,
    #[serde(default = "default_serving_maximum_queue_bytes")]
    pub maximum_queue_bytes: u32,
    #[serde(default = "default_serving_rx_timeout_us")]
    pub rx_timeout_us: u32,
    #[serde(default = "default_serving_rx_queue_blocks")]
    pub rx_queue_blocks: u16,
    #[serde(default = "default_serving_startup_lead_ms")]
    pub startup_lead_ms: u32,
    #[serde(default = "default_serving_minimum_submission_us")]
    pub minimum_submission_us: u32,
    #[serde(default = "default_serving_maximum_submission_us")]
    pub maximum_submission_us: u32,
    #[serde(default = "default_serving_maximum_preparation_us")]
    pub maximum_preparation_us: u32,
    #[serde(default = "default_serving_idle_wait_us")]
    pub idle_wait_us: u32,
    #[serde(default = "default_serving_controller_idle_wait_us")]
    pub controller_idle_wait_us: Option<u32>,
    #[serde(default = "default_serving_startup_timeout_ms")]
    pub startup_timeout_ms: u32,
    #[serde(default = "default_serving_clock_stall_timeout_ms")]
    pub clock_stall_timeout_ms: u32,
    #[serde(default = "default_serving_join_timeout_ms")]
    pub join_timeout_ms: u32,
}
impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            dch_cpu: Default::default(),
            scheduling: Default::default(),
            simd: Default::default(),
            media: Default::default(),
            clock: Default::default(),
            recovery: Default::default(),
            spin_wait: Default::default(),
            lock_memory: Default::default(),
            tx_batch_slots: default_serving_tx_batch_slots(),
            maximum_ues: default_serving_maximum_ues(),
            maximum_sdu_bytes: default_serving_maximum_sdu_bytes(),
            queued_sdus_per_bearer: default_serving_queued_sdus_per_bearer(),
            maximum_queue_bytes: default_serving_maximum_queue_bytes(),
            rx_timeout_us: default_serving_rx_timeout_us(),
            rx_queue_blocks: default_serving_rx_queue_blocks(),
            startup_lead_ms: default_serving_startup_lead_ms(),
            minimum_submission_us: default_serving_minimum_submission_us(),
            maximum_submission_us: default_serving_maximum_submission_us(),
            maximum_preparation_us: default_serving_maximum_preparation_us(),
            idle_wait_us: default_serving_idle_wait_us(),
            controller_idle_wait_us: default_serving_controller_idle_wait_us(),
            startup_timeout_ms: default_serving_startup_timeout_ms(),
            clock_stall_timeout_ms: default_serving_clock_stall_timeout_ms(),
            join_timeout_ms: default_serving_join_timeout_ms(),
        }
    }
}
impl RuntimeSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.scheduling).validate_schema()?;
        (self.media).validate_schema()?;
        (self.clock).validate_schema()?;
        (self.recovery).validate_schema()?;
        if !(1..=2).contains(&self.tx_batch_slots) {
            anyhow::bail!("RuntimeSettings.tx_batch_slots outside 1..=2");
        }
        if !(1..=64).contains(&self.maximum_ues) {
            anyhow::bail!("RuntimeSettings.maximum_ues outside 1..=64");
        }
        if !(1..=65536).contains(&self.maximum_sdu_bytes) {
            anyhow::bail!("RuntimeSettings.maximum_sdu_bytes outside 1..=65536");
        }
        if !(1..=256).contains(&self.queued_sdus_per_bearer) {
            anyhow::bail!("RuntimeSettings.queued_sdus_per_bearer outside 1..=256");
        }
        if !(1048576..=536870912).contains(&self.maximum_queue_bytes) {
            anyhow::bail!("RuntimeSettings.maximum_queue_bytes outside 1048576..=536870912");
        }
        if !(1..=10000).contains(&self.rx_timeout_us) {
            anyhow::bail!("RuntimeSettings.rx_timeout_us outside 1..=10000");
        }
        if !(1..=128).contains(&self.rx_queue_blocks) {
            anyhow::bail!("RuntimeSettings.rx_queue_blocks outside 1..=128");
        }
        if !(50..=5000).contains(&self.startup_lead_ms) {
            anyhow::bail!("RuntimeSettings.startup_lead_ms outside 50..=5000");
        }
        if !(1..=1000000).contains(&self.minimum_submission_us) {
            anyhow::bail!("RuntimeSettings.minimum_submission_us outside 1..=1000000");
        }
        if !(1..=1000000).contains(&self.maximum_submission_us) {
            anyhow::bail!("RuntimeSettings.maximum_submission_us outside 1..=1000000");
        }
        if !(1..=1000000).contains(&self.maximum_preparation_us) {
            anyhow::bail!("RuntimeSettings.maximum_preparation_us outside 1..=1000000");
        }
        if !(1..=1000).contains(&self.idle_wait_us) {
            anyhow::bail!("RuntimeSettings.idle_wait_us outside 1..=1000");
        }
        if let Some(value) = &self.controller_idle_wait_us {
            if !(1..=1000).contains(value) {
                anyhow::bail!("RuntimeSettings.controller_idle_wait_us outside 1..=1000");
            }
        }
        if !(1..=60000).contains(&self.startup_timeout_ms) {
            anyhow::bail!("RuntimeSettings.startup_timeout_ms outside 1..=60000");
        }
        if !(1..=60000).contains(&self.clock_stall_timeout_ms) {
            anyhow::bail!("RuntimeSettings.clock_stall_timeout_ms outside 1..=60000");
        }
        if !(1..=60000).contains(&self.join_timeout_ms) {
            anyhow::bail!("RuntimeSettings.join_timeout_ms outside 1..=60000");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RadioSettings {
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default)]
    pub soapy_args: String,
    #[serde(default)]
    pub uhd_args: String,
    #[doc = "TX antenna port. Required on Soapy. Absent selects TX/RX on UHD or the driver default on bladeRF."]
    #[serde(default)]
    pub tx_antenna: Option<String>,
    #[doc = "RX antenna port. Required on Soapy. Absent selects RX2 on UHD or the driver default on bladeRF."]
    #[serde(default)]
    pub rx_antenna: Option<String>,
    #[serde(default)]
    pub uhd_rx_timestamp_offset: Option<i64>,
    #[serde(default)]
    pub bladerf_args: String,
    #[serde(default)]
    pub bladerf_rx_sample_phase: u8,
    pub uarfcn_dl: u32,
    pub uarfcn_ul: u32,
    pub tx_gain_db: f64,
    pub rx_gain_db: f64,
    #[doc = "Receive path delay of the radio in RX samples, added to the 3GPP uplink timing. Depends on the radio and its sample geometry."]
    #[serde(default)]
    pub rx_delay_samples: f64,
    #[doc = "TX samples per 3.84 Mcps chip, either 2 or 4. RX remains at 2 samples per chip. Applied TX and RX chip rates must agree before activation."]
    #[serde(default = "default_tx_samples_per_chip")]
    pub tx_samples_per_chip: u8,
    #[doc = "The analog TX passband must encompass the 5 MHz signal and both sides of any LO offset."]
    #[serde(default = "default_tx_analog_bandwidth_hz")]
    pub tx_analog_bandwidth_hz: u32,
    #[doc = "TX LO offset with backend frequency compensation. The on-air carrier is unchanged. Nonzero offsets require backend support and sufficient analog and device-clock bandwidth.\nUnset with cell.packet_modes selects 2.5 MHz for a single carrier on UHD and 0 otherwise."]
    #[serde(default)]
    pub tx_lo_offset_hz: i64,
    #[serde(default = "default_bladerf_stream_buffers")]
    pub bladerf_stream_buffers: u32,
    #[serde(default = "default_bladerf_stream_buffer_samples")]
    pub bladerf_stream_buffer_samples: u32,
    #[serde(default = "default_bladerf_stream_transfers")]
    pub bladerf_stream_transfers: u32,
    #[serde(default)]
    pub bladerf_rx_timestamp_offset: Option<i64>,
}
impl RadioSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(self.tx_gain_db).is_finite() {
            anyhow::bail!("RadioSettings.tx_gain_db must be finite");
        }
        if !(self.rx_gain_db).is_finite() {
            anyhow::bail!("RadioSettings.rx_gain_db must be finite");
        }
        if !(self.rx_delay_samples).is_finite() {
            anyhow::bail!("RadioSettings.rx_delay_samples must be finite");
        }
        if !(-1000000.0..=1000000.0).contains(&self.rx_delay_samples) {
            anyhow::bail!("RadioSettings.rx_delay_samples outside -1000000.0..=1000000.0");
        }
        if !(2..=4).contains(&self.tx_samples_per_chip) {
            anyhow::bail!("RadioSettings.tx_samples_per_chip outside 2..=4");
        }
        if ((self.tx_samples_per_chip as i128) - 2_i128).rem_euclid(2_i128) != 0 {
            anyhow::bail!("RadioSettings.tx_samples_per_chip must advance in steps of 2 from 2");
        }
        if !(5000000..=56000000).contains(&self.tx_analog_bandwidth_hz) {
            anyhow::bail!("RadioSettings.tx_analog_bandwidth_hz outside 5000000..=56000000");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ReceiverDedicatedSettings {
    #[serde(default = "default_dedicated_shared_bytes")]
    pub maximum_shared_bytes: u32,
    #[serde(default = "default_dedicated_sample_bytes")]
    pub maximum_sample_bytes: u32,
    #[serde(default = "default_dedicated_coded_bits")]
    pub maximum_coded_bits: u32,
    #[serde(default = "default_dedicated_output_bytes")]
    pub maximum_output_bytes: u32,
    #[serde(default = "default_dedicated_demultiplexed_bits")]
    pub maximum_demultiplexed_bits: u32,
    #[serde(default = "default_dedicated_bearer_bytes")]
    pub maximum_bearer_bytes: u32,
    #[serde(default = "default_dedicated_transmit_bits")]
    pub maximum_transmit_bits: u32,
    #[serde(default = "default_dedicated_frame_bits")]
    pub maximum_frame_bits: u32,
    #[serde(default = "default_dedicated_history_frames")]
    pub history_frames: u8,
    #[serde(default = "default_dedicated_work_per_cycle")]
    pub work_per_cycle: u16,
    #[serde(default = "default_dedicated_command_capacity")]
    pub command_capacity: u16,
    #[serde(default = "default_dedicated_output_slots")]
    pub output_slots: u16,
    #[serde(default = "default_dedicated_search_radius_samples")]
    pub search_radius_samples: u16,
    #[serde(default = "default_dedicated_minimum_coherent_fraction")]
    pub minimum_coherent_fraction: f64,
    #[serde(default = "default_dedicated_carrier_initial_hz")]
    pub carrier_initial_hz: f64,
    #[serde(default = "default_dedicated_carrier_maximum_absolute_hz")]
    pub carrier_maximum_absolute_hz: f64,
    #[serde(default = "default_dedicated_carrier_update_fraction")]
    pub carrier_update_fraction: f64,
    #[serde(default = "default_dedicated_carrier_minimum_coherence")]
    pub carrier_minimum_coherence: f64,
    #[serde(default = "default_dedicated_likelihood_maximum")]
    pub likelihood_maximum: f64,
    #[serde(default = "default_dedicated_tfci_minimum_score")]
    pub tfci_minimum_score: f64,
    #[serde(default = "default_dedicated_tfci_minimum_margin")]
    pub tfci_minimum_margin: f64,
    #[serde(default = "default_dedicated_radio_link_out_of_sync_frames")]
    pub radio_link_out_of_sync_frames: u32,
    #[serde(default = "default_dedicated_radio_link_in_sync_frames")]
    pub radio_link_in_sync_frames: u32,
    #[serde(default = "default_dedicated_radio_link_failure_frames")]
    pub radio_link_failure_frames: u32,
}
impl Default for ReceiverDedicatedSettings {
    fn default() -> Self {
        Self {
            maximum_shared_bytes: default_dedicated_shared_bytes(),
            maximum_sample_bytes: default_dedicated_sample_bytes(),
            maximum_coded_bits: default_dedicated_coded_bits(),
            maximum_output_bytes: default_dedicated_output_bytes(),
            maximum_demultiplexed_bits: default_dedicated_demultiplexed_bits(),
            maximum_bearer_bytes: default_dedicated_bearer_bytes(),
            maximum_transmit_bits: default_dedicated_transmit_bits(),
            maximum_frame_bits: default_dedicated_frame_bits(),
            history_frames: default_dedicated_history_frames(),
            work_per_cycle: default_dedicated_work_per_cycle(),
            command_capacity: default_dedicated_command_capacity(),
            output_slots: default_dedicated_output_slots(),
            search_radius_samples: default_dedicated_search_radius_samples(),
            minimum_coherent_fraction: default_dedicated_minimum_coherent_fraction(),
            carrier_initial_hz: default_dedicated_carrier_initial_hz(),
            carrier_maximum_absolute_hz: default_dedicated_carrier_maximum_absolute_hz(),
            carrier_update_fraction: default_dedicated_carrier_update_fraction(),
            carrier_minimum_coherence: default_dedicated_carrier_minimum_coherence(),
            likelihood_maximum: default_dedicated_likelihood_maximum(),
            tfci_minimum_score: default_dedicated_tfci_minimum_score(),
            tfci_minimum_margin: default_dedicated_tfci_minimum_margin(),
            radio_link_out_of_sync_frames: default_dedicated_radio_link_out_of_sync_frames(),
            radio_link_in_sync_frames: default_dedicated_radio_link_in_sync_frames(),
            radio_link_failure_frames: default_dedicated_radio_link_failure_frames(),
        }
    }
}
impl ReceiverDedicatedSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(1..=536870912).contains(&self.maximum_shared_bytes) {
            anyhow::bail!("ReceiverDedicatedSettings.maximum_shared_bytes outside 1..=536870912");
        }
        if !(1..=536870912).contains(&self.maximum_sample_bytes) {
            anyhow::bail!("ReceiverDedicatedSettings.maximum_sample_bytes outside 1..=536870912");
        }
        if !(1..=536870912).contains(&self.maximum_coded_bits) {
            anyhow::bail!("ReceiverDedicatedSettings.maximum_coded_bits outside 1..=536870912");
        }
        if !(1..=536870912).contains(&self.maximum_output_bytes) {
            anyhow::bail!("ReceiverDedicatedSettings.maximum_output_bytes outside 1..=536870912");
        }
        if !(1..=536870912).contains(&self.maximum_demultiplexed_bits) {
            anyhow::bail!(
                "ReceiverDedicatedSettings.maximum_demultiplexed_bits outside 1..=536870912"
            );
        }
        if !(1..=536870912).contains(&self.maximum_bearer_bytes) {
            anyhow::bail!("ReceiverDedicatedSettings.maximum_bearer_bytes outside 1..=536870912");
        }
        if !(1..=536870912).contains(&self.maximum_transmit_bits) {
            anyhow::bail!("ReceiverDedicatedSettings.maximum_transmit_bits outside 1..=536870912");
        }
        if !(1..=1048576).contains(&self.maximum_frame_bits) {
            anyhow::bail!("ReceiverDedicatedSettings.maximum_frame_bits outside 1..=1048576");
        }
        if !(1..=8).contains(&self.history_frames) {
            anyhow::bail!("ReceiverDedicatedSettings.history_frames outside 1..=8");
        }
        if !(1..=256).contains(&self.work_per_cycle) {
            anyhow::bail!("ReceiverDedicatedSettings.work_per_cycle outside 1..=256");
        }
        if !(1..=256).contains(&self.command_capacity) {
            anyhow::bail!("ReceiverDedicatedSettings.command_capacity outside 1..=256");
        }
        if !(1..=256).contains(&self.output_slots) {
            anyhow::bail!("ReceiverDedicatedSettings.output_slots outside 1..=256");
        }
        if !(1..=256).contains(&self.search_radius_samples) {
            anyhow::bail!("ReceiverDedicatedSettings.search_radius_samples outside 1..=256");
        }
        if !(self.minimum_coherent_fraction).is_finite() {
            anyhow::bail!("ReceiverDedicatedSettings.minimum_coherent_fraction must be finite");
        }
        if !(1e-06..=1.0).contains(&self.minimum_coherent_fraction) {
            anyhow::bail!(
                "ReceiverDedicatedSettings.minimum_coherent_fraction outside 1e-06..=1.0"
            );
        }
        if !(self.carrier_initial_hz).is_finite() {
            anyhow::bail!("ReceiverDedicatedSettings.carrier_initial_hz must be finite");
        }
        if !(-100000.0..=100000.0).contains(&self.carrier_initial_hz) {
            anyhow::bail!(
                "ReceiverDedicatedSettings.carrier_initial_hz outside -100000.0..=100000.0"
            );
        }
        if !(self.carrier_maximum_absolute_hz).is_finite() {
            anyhow::bail!("ReceiverDedicatedSettings.carrier_maximum_absolute_hz must be finite");
        }
        if !(1.0..=100000.0).contains(&self.carrier_maximum_absolute_hz) {
            anyhow::bail!(
                "ReceiverDedicatedSettings.carrier_maximum_absolute_hz outside 1.0..=100000.0"
            );
        }
        if !(self.carrier_update_fraction).is_finite() {
            anyhow::bail!("ReceiverDedicatedSettings.carrier_update_fraction must be finite");
        }
        if !(1e-06..=1.0).contains(&self.carrier_update_fraction) {
            anyhow::bail!("ReceiverDedicatedSettings.carrier_update_fraction outside 1e-06..=1.0");
        }
        if !(self.carrier_minimum_coherence).is_finite() {
            anyhow::bail!("ReceiverDedicatedSettings.carrier_minimum_coherence must be finite");
        }
        if !(1e-06..=1.0).contains(&self.carrier_minimum_coherence) {
            anyhow::bail!(
                "ReceiverDedicatedSettings.carrier_minimum_coherence outside 1e-06..=1.0"
            );
        }
        if !(self.likelihood_maximum).is_finite() {
            anyhow::bail!("ReceiverDedicatedSettings.likelihood_maximum must be finite");
        }
        if !(1e-06..=3.4028235e+38).contains(&self.likelihood_maximum) {
            anyhow::bail!(
                "ReceiverDedicatedSettings.likelihood_maximum outside 1e-06..=3.4028235e+38"
            );
        }
        if !(self.tfci_minimum_score).is_finite() {
            anyhow::bail!("ReceiverDedicatedSettings.tfci_minimum_score must be finite");
        }
        if !(0.0..=1.0).contains(&self.tfci_minimum_score) {
            anyhow::bail!("ReceiverDedicatedSettings.tfci_minimum_score outside 0.0..=1.0");
        }
        if !(self.tfci_minimum_margin).is_finite() {
            anyhow::bail!("ReceiverDedicatedSettings.tfci_minimum_margin must be finite");
        }
        if !(0.0..=2.0).contains(&self.tfci_minimum_margin) {
            anyhow::bail!("ReceiverDedicatedSettings.tfci_minimum_margin outside 0.0..=2.0");
        }
        if !(1..=256).contains(&self.radio_link_out_of_sync_frames) {
            anyhow::bail!(
                "ReceiverDedicatedSettings.radio_link_out_of_sync_frames outside 1..=256"
            );
        }
        if !(1..=256).contains(&self.radio_link_in_sync_frames) {
            anyhow::bail!("ReceiverDedicatedSettings.radio_link_in_sync_frames outside 1..=256");
        }
        if !(0..=2550).contains(&self.radio_link_failure_frames) {
            anyhow::bail!("ReceiverDedicatedSettings.radio_link_failure_frames outside 0..=2550");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ReceiverSettings {
    #[serde(default)]
    pub prach: crate::PrachReceiverSettings,
    #[serde(default = "default_turbo_iterations")]
    pub turbo_iterations: u32,
    #[serde(default = "default_serving_filter_span_chips")]
    pub filter_span_chips: u16,
    #[serde(default)]
    pub dedicated: ReceiverDedicatedSettings,
}
impl Default for ReceiverSettings {
    fn default() -> Self {
        Self {
            prach: Default::default(),
            turbo_iterations: default_turbo_iterations(),
            filter_span_chips: default_serving_filter_span_chips(),
            dedicated: Default::default(),
        }
    }
}
impl ReceiverSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        self.prach.validate_schema()?;
        if !(4..=128).contains(&self.filter_span_chips) {
            anyhow::bail!("ReceiverSettings.filter_span_chips outside 4..=128");
        }
        if ((self.filter_span_chips as i128) - 4_i128).rem_euclid(2_i128) != 0 {
            anyhow::bail!("ReceiverSettings.filter_span_chips must advance in steps of 2 from 4");
        }
        (self.dedicated).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PowerControlUplinkSettings {
    #[serde(default = "default_hspa_uplink_dpcch_sir_target_db")]
    pub sir_ceiling_db: f64,
    #[serde(default)]
    pub initial_sir_db: Option<f64>,
    #[serde(default)]
    pub sir_floor_db: Option<f64>,
    #[serde(default = "default_hspa_uplink_target_bler")]
    pub target_bler: f64,
    #[serde(default = "default_uplink_dpcch_tpc_hold_groups")]
    pub tpc_hold_groups: u8,
}
impl Default for PowerControlUplinkSettings {
    fn default() -> Self {
        Self {
            sir_ceiling_db: default_hspa_uplink_dpcch_sir_target_db(),
            initial_sir_db: Default::default(),
            sir_floor_db: Default::default(),
            target_bler: default_hspa_uplink_target_bler(),
            tpc_hold_groups: default_uplink_dpcch_tpc_hold_groups(),
        }
    }
}
impl PowerControlUplinkSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(self.sir_ceiling_db).is_finite() {
            anyhow::bail!("PowerControlUplinkSettings.sir_ceiling_db must be finite");
        }
        if let Some(value) = &self.initial_sir_db {
            if !(value).is_finite() {
                anyhow::bail!("PowerControlUplinkSettings.initial_sir_db must be finite");
            }
            if !(0.0..=45.0).contains(value) {
                anyhow::bail!("PowerControlUplinkSettings.initial_sir_db outside 0.0..=45.0");
            }
        }
        if let Some(value) = &self.sir_floor_db {
            if !(value).is_finite() {
                anyhow::bail!("PowerControlUplinkSettings.sir_floor_db must be finite");
            }
        }
        if !(self.target_bler).is_finite() {
            anyhow::bail!("PowerControlUplinkSettings.target_bler must be finite");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RustDownlinkPowerConfig {
    #[doc = "Enables bounded filtered waveform peak reduction. Intentionally distorts the composite, adds a fixed 64-chip delay and rejects excessive distortion. Requires independent modulation, spectrum and signaling validation before use. Disabled by default."]
    #[serde(default)]
    pub crest_reduction_enabled: bool,
    #[doc = "Fixed RMS reference for all carrier power percentages. Never automatically rescaled. Final pulse-shaped I/Q is checked against the configured peak margin before submission."]
    #[serde(default = "default_downlink_power_carrier_rms")]
    pub carrier_rms: f32,
    #[doc = "Fixed margin below unit I/Q component full scale. Not an RF linearity calibration."]
    #[serde(default = "default_downlink_power_peak_margin_db")]
    pub peak_margin_db: f32,
    #[doc = "Continuous pilot share of the configured carrier power budget."]
    #[serde(default = "default_downlink_power_cpich_percent")]
    pub cpich_percent: f32,
    #[doc = "Combined P-CCPCH and synchronization allocation, averaged over a radio frame."]
    #[serde(default = "default_downlink_power_broadcast_percent")]
    pub broadcast_percent: f32,
    #[doc = "Share of the broadcast allocation reserved jointly for P-SCH and S-SCH, split equally and averaged over a radio frame. The remainder goes to P-CCPCH. Does not change the total broadcast allocation."]
    #[serde(default = "default_downlink_power_broadcast_sync_percent")]
    pub broadcast_sync_percent: f32,
    #[doc = "Reserved common packet-data channel power."]
    #[serde(default = "default_downlink_power_fach_percent")]
    pub fach_percent: f32,
    #[doc = "Reserved common paging-data channel power."]
    #[serde(default = "default_downlink_power_pch_percent")]
    pub pch_percent: f32,
    #[doc = "Paging-indicator reserve. Actual power is quantized to the signaled whole-dB offset."]
    #[serde(default = "default_downlink_power_pich_percent")]
    pub pich_percent: f32,
    #[doc = "Per-acquisition-indicator reserve. Actual power is quantized to the signaled whole-dB offset."]
    #[serde(default = "default_downlink_power_aich_percent")]
    pub aich_percent: f32,
    #[doc = "Cell-wide dedicated average-power reserve across configured UE/code slots. Accounts for boosted TPC using the worst valid slot duty factor, not its instantaneous peak as continuous power."]
    #[serde(default = "default_downlink_power_dedicated_percent")]
    pub dedicated_percent: f32,
    #[doc = "Cell-wide HS-PDSCH power reserve, divided among simultaneous transmission slots admitted by the shared HS-SCCH pool, not registered UE capacity."]
    #[serde(default = "default_downlink_power_hsdpa_percent")]
    pub hsdpa_percent: f32,
    #[doc = "Cell-wide HS-SCCH reserve."]
    #[serde(default = "default_downlink_power_hs_scch_percent")]
    pub hs_scch_percent: f32,
    #[doc = "Cell-wide E-AGCH reserve."]
    #[serde(default = "default_downlink_power_e_agch_percent")]
    pub e_agch_percent: f32,
    #[doc = "Cell-wide E-HICH reserve."]
    #[serde(default = "default_downlink_power_e_hich_percent")]
    pub e_hich_percent: f32,
}
impl Default for RustDownlinkPowerConfig {
    fn default() -> Self {
        Self {
            crest_reduction_enabled: Default::default(),
            carrier_rms: default_downlink_power_carrier_rms(),
            peak_margin_db: default_downlink_power_peak_margin_db(),
            cpich_percent: default_downlink_power_cpich_percent(),
            broadcast_percent: default_downlink_power_broadcast_percent(),
            broadcast_sync_percent: default_downlink_power_broadcast_sync_percent(),
            fach_percent: default_downlink_power_fach_percent(),
            pch_percent: default_downlink_power_pch_percent(),
            pich_percent: default_downlink_power_pich_percent(),
            aich_percent: default_downlink_power_aich_percent(),
            dedicated_percent: default_downlink_power_dedicated_percent(),
            hsdpa_percent: default_downlink_power_hsdpa_percent(),
            hs_scch_percent: default_downlink_power_hs_scch_percent(),
            e_agch_percent: default_downlink_power_e_agch_percent(),
            e_hich_percent: default_downlink_power_e_hich_percent(),
        }
    }
}
impl RustDownlinkPowerConfig {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(self.carrier_rms).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.carrier_rms must be finite");
        }
        if !(0.001..=1.0).contains(&self.carrier_rms) {
            anyhow::bail!("RustDownlinkPowerConfig.carrier_rms outside 0.001..=1.0");
        }
        if !(self.peak_margin_db).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.peak_margin_db must be finite");
        }
        if !(0.1..=20.0).contains(&self.peak_margin_db) {
            anyhow::bail!("RustDownlinkPowerConfig.peak_margin_db outside 0.1..=20.0");
        }
        if !(self.cpich_percent).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.cpich_percent must be finite");
        }
        if !(0.001..=100.0).contains(&self.cpich_percent) {
            anyhow::bail!("RustDownlinkPowerConfig.cpich_percent outside 0.001..=100.0");
        }
        if !(self.broadcast_percent).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.broadcast_percent must be finite");
        }
        if !(0.001..=100.0).contains(&self.broadcast_percent) {
            anyhow::bail!("RustDownlinkPowerConfig.broadcast_percent outside 0.001..=100.0");
        }
        if !(self.broadcast_sync_percent).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.broadcast_sync_percent must be finite");
        }
        if !(0.1..=99.9).contains(&self.broadcast_sync_percent) {
            anyhow::bail!("RustDownlinkPowerConfig.broadcast_sync_percent outside 0.1..=99.9");
        }
        if !(self.fach_percent).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.fach_percent must be finite");
        }
        if !(0.001..=100.0).contains(&self.fach_percent) {
            anyhow::bail!("RustDownlinkPowerConfig.fach_percent outside 0.001..=100.0");
        }
        if !(self.pch_percent).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.pch_percent must be finite");
        }
        if !(0.001..=100.0).contains(&self.pch_percent) {
            anyhow::bail!("RustDownlinkPowerConfig.pch_percent outside 0.001..=100.0");
        }
        if !(self.pich_percent).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.pich_percent must be finite");
        }
        if !(0.001..=100.0).contains(&self.pich_percent) {
            anyhow::bail!("RustDownlinkPowerConfig.pich_percent outside 0.001..=100.0");
        }
        if !(self.aich_percent).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.aich_percent must be finite");
        }
        if !(0.001..=100.0).contains(&self.aich_percent) {
            anyhow::bail!("RustDownlinkPowerConfig.aich_percent outside 0.001..=100.0");
        }
        if !(self.dedicated_percent).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.dedicated_percent must be finite");
        }
        if !(0.001..=100.0).contains(&self.dedicated_percent) {
            anyhow::bail!("RustDownlinkPowerConfig.dedicated_percent outside 0.001..=100.0");
        }
        if !(self.hsdpa_percent).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.hsdpa_percent must be finite");
        }
        if !(0.0..=100.0).contains(&self.hsdpa_percent) {
            anyhow::bail!("RustDownlinkPowerConfig.hsdpa_percent outside 0.0..=100.0");
        }
        if !(self.hs_scch_percent).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.hs_scch_percent must be finite");
        }
        if !(0.0..=100.0).contains(&self.hs_scch_percent) {
            anyhow::bail!("RustDownlinkPowerConfig.hs_scch_percent outside 0.0..=100.0");
        }
        if !(self.e_agch_percent).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.e_agch_percent must be finite");
        }
        if !(0.0..=100.0).contains(&self.e_agch_percent) {
            anyhow::bail!("RustDownlinkPowerConfig.e_agch_percent outside 0.0..=100.0");
        }
        if !(self.e_hich_percent).is_finite() {
            anyhow::bail!("RustDownlinkPowerConfig.e_hich_percent must be finite");
        }
        if !(0.0..=100.0).contains(&self.e_hich_percent) {
            anyhow::bail!("RustDownlinkPowerConfig.e_hich_percent outside 0.0..=100.0");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PowerControlDownlinkDedicatedSettings {
    #[serde(default = "default_dedicated_downlink_offset_db")]
    pub downlink_offset_db: f32,
    #[serde(default = "default_dedicated_downlink_minimum_offset_db")]
    pub downlink_minimum_offset_db: f32,
    #[serde(default = "default_dedicated_tpc_offset_db")]
    pub tpc_offset_db: f32,
    #[serde(default = "default_dedicated_tpc_maximum_observation_age_chips")]
    pub tpc_maximum_observation_age_chips: u32,
    #[serde(default = "default_dedicated_tpc_initial_maximum_slots")]
    pub tpc_initial_maximum_slots: u32,
}
impl Default for PowerControlDownlinkDedicatedSettings {
    fn default() -> Self {
        Self {
            downlink_offset_db: default_dedicated_downlink_offset_db(),
            downlink_minimum_offset_db: default_dedicated_downlink_minimum_offset_db(),
            tpc_offset_db: default_dedicated_tpc_offset_db(),
            tpc_maximum_observation_age_chips: default_dedicated_tpc_maximum_observation_age_chips(
            ),
            tpc_initial_maximum_slots: default_dedicated_tpc_initial_maximum_slots(),
        }
    }
}
impl PowerControlDownlinkDedicatedSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(self.downlink_offset_db).is_finite() {
            anyhow::bail!(
                "PowerControlDownlinkDedicatedSettings.downlink_offset_db must be finite"
            );
        }
        if !(-20.0..=6.0).contains(&self.downlink_offset_db) {
            anyhow::bail!(
                "PowerControlDownlinkDedicatedSettings.downlink_offset_db outside -20.0..=6.0"
            );
        }
        if !(self.downlink_minimum_offset_db).is_finite() {
            anyhow::bail!(
                "PowerControlDownlinkDedicatedSettings.downlink_minimum_offset_db must be finite"
            );
        }
        if !(-54.0..=6.0).contains(&self.downlink_minimum_offset_db) {
            anyhow::bail!(
                "PowerControlDownlinkDedicatedSettings.downlink_minimum_offset_db outside -54.0..=6.0"
            );
        }
        if !(self.tpc_offset_db).is_finite() {
            anyhow::bail!("PowerControlDownlinkDedicatedSettings.tpc_offset_db must be finite");
        }
        if !(0.0..=12.0).contains(&self.tpc_offset_db) {
            anyhow::bail!("PowerControlDownlinkDedicatedSettings.tpc_offset_db outside 0.0..=12.0");
        }
        if !(1..=4294967295).contains(&self.tpc_maximum_observation_age_chips) {
            anyhow::bail!(
                "PowerControlDownlinkDedicatedSettings.tpc_maximum_observation_age_chips outside 1..=4294967295"
            );
        }
        if !(1..=4294967295).contains(&self.tpc_initial_maximum_slots) {
            anyhow::bail!(
                "PowerControlDownlinkDedicatedSettings.tpc_initial_maximum_slots outside 1..=4294967295"
            );
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PowerControlDownlinkSettings {
    #[serde(default = "default_pich_power_offset_db")]
    pub pich_power_offset_db: i8,
    #[serde(default)]
    pub allocation: Option<RustDownlinkPowerConfig>,
    #[serde(default = "default_serving_cpich_amplitude")]
    pub cpich_amplitude: f32,
    #[serde(default = "default_serving_common_channel_offset_db")]
    pub common_channel_offset_db: f32,
    #[serde(default)]
    pub dedicated: PowerControlDownlinkDedicatedSettings,
}
impl Default for PowerControlDownlinkSettings {
    fn default() -> Self {
        Self {
            pich_power_offset_db: default_pich_power_offset_db(),
            allocation: Default::default(),
            cpich_amplitude: default_serving_cpich_amplitude(),
            common_channel_offset_db: default_serving_common_channel_offset_db(),
            dedicated: Default::default(),
        }
    }
}
impl PowerControlDownlinkSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if let Some(value) = &self.allocation {
            (value).validate_schema()?;
        }
        if !(self.cpich_amplitude).is_finite() {
            anyhow::bail!("PowerControlDownlinkSettings.cpich_amplitude must be finite");
        }
        if !(0.0..=0.25).contains(&self.cpich_amplitude) {
            anyhow::bail!("PowerControlDownlinkSettings.cpich_amplitude outside 0.0..=0.25");
        }
        if !(self.common_channel_offset_db).is_finite() {
            anyhow::bail!("PowerControlDownlinkSettings.common_channel_offset_db must be finite");
        }
        if !(-20.0..=6.0).contains(&self.common_channel_offset_db) {
            anyhow::bail!(
                "PowerControlDownlinkSettings.common_channel_offset_db outside -20.0..=6.0"
            );
        }
        (self.dedicated).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct PowerControlSettings {
    #[serde(default)]
    pub uplink: PowerControlUplinkSettings,
    #[serde(default)]
    pub downlink: PowerControlDownlinkSettings,
}
impl PowerControlSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.uplink).validate_schema()?;
        (self.downlink).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct HspaDownlinkFeedbackSettings {
    #[serde(default = "default_hspa_hs_dpcch_ack_nack_delta")]
    pub ack_nack_power_index: u8,
    #[serde(default = "default_hspa_hs_dpcch_cqi_delta")]
    pub cqi_power_index: u8,
    #[serde(default = "default_hspa_cqi_feedback_cycle_ms")]
    pub cqi_interval_ms: u16,
}
impl Default for HspaDownlinkFeedbackSettings {
    fn default() -> Self {
        Self {
            ack_nack_power_index: default_hspa_hs_dpcch_ack_nack_delta(),
            cqi_power_index: default_hspa_hs_dpcch_cqi_delta(),
            cqi_interval_ms: default_hspa_cqi_feedback_cycle_ms(),
        }
    }
}
impl HspaDownlinkFeedbackSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(0..=8).contains(&self.cqi_power_index) {
            anyhow::bail!("HspaDownlinkFeedbackSettings.cqi_power_index outside 0..=8");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RustHsdpaEvidence {
    #[doc = "Minimum normalized correlation for accepted feedback."]
    pub minimum_correlation: f64,
    #[doc = "Minimum normalized separation from the next feedback hypothesis."]
    pub minimum_margin: f64,
    #[doc = "Minimum coherent pilot fraction."]
    pub minimum_pilot_fraction: f64,
    #[doc = "Finite nonnegative maximum pilot-normalized disturbance."]
    pub maximum_relative_disturbance: f64,
    #[doc = "Finite positive minimum pilot-normalized feedback soft-symbol power."]
    pub minimum_relative_soft_power: f64,
}
impl RustHsdpaEvidence {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(self.minimum_correlation).is_finite() {
            anyhow::bail!("RustHsdpaEvidence.minimum_correlation must be finite");
        }
        if !(0.0..=1.0).contains(&self.minimum_correlation) {
            anyhow::bail!("RustHsdpaEvidence.minimum_correlation outside 0.0..=1.0");
        }
        if !(self.minimum_margin).is_finite() {
            anyhow::bail!("RustHsdpaEvidence.minimum_margin must be finite");
        }
        if !(0.0..=2.0).contains(&self.minimum_margin) {
            anyhow::bail!("RustHsdpaEvidence.minimum_margin outside 0.0..=2.0");
        }
        if !(self.minimum_pilot_fraction).is_finite() {
            anyhow::bail!("RustHsdpaEvidence.minimum_pilot_fraction must be finite");
        }
        if !(0.0..=1.0).contains(&self.minimum_pilot_fraction) {
            anyhow::bail!("RustHsdpaEvidence.minimum_pilot_fraction outside 0.0..=1.0");
        }
        if !(self.maximum_relative_disturbance).is_finite() {
            anyhow::bail!("RustHsdpaEvidence.maximum_relative_disturbance must be finite");
        }
        if !(self.minimum_relative_soft_power).is_finite() {
            anyhow::bail!("RustHsdpaEvidence.minimum_relative_soft_power must be finite");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RustHsdpaScheduling {
    #[doc = "Maximum cached transmission plans per UE carrier."]
    pub maximum_formats: u32,
    #[doc = "Maximum coded transport bits per HARQ buffer."]
    pub maximum_coded_bits: u32,
    #[doc = "Retained transmission-plan storage limit per UE carrier."]
    pub maximum_plan_bytes: u32,
    #[doc = "Combined coded-buffer storage limit per UE carrier."]
    pub maximum_buffer_bytes: u32,
    #[doc = "Retained encoder scratch and interleaver storage limit per UE carrier."]
    pub maximum_encoder_bytes: u32,
    #[doc = "Aggregate retained scheduler storage limit per UE across all configured carriers; split equally for dual-cell admission."]
    pub maximum_ue_bytes: u32,
    #[doc = "Shared cell admission budget for retained scheduler resources and outstanding carrier preparations."]
    pub maximum_cell_bytes: u32,
    #[doc = "Maximum accepted CQI age in UMTS chips."]
    pub maximum_cqi_age_chips: u32,
    #[doc = "HSDPA packet preparation lead before HS-SCCH, independent of Rel99 bearer activation."]
    #[serde(default = "default_hsdpa_preparation_guard_us")]
    pub preparation_guard_us: u32,
    #[doc = "Maximum plan candidates examined per scheduling call."]
    pub selection_budget: u32,
    #[doc = "Maximum outer-loop CQI adjustment above or below the reported CQI. Zero disables adaptation."]
    #[serde(default = "default_hsdpa_cqi_offset_limit")]
    pub cqi_offset_limit: u8,
    #[doc = "First-transmission block error rate that the outer-loop CQI adjustment converges to."]
    #[serde(default = "default_hsdpa_cqi_target_bler_percent")]
    pub cqi_target_bler_percent: u8,
    #[doc = "HS-SCCH complex-chip RMS amplitude."]
    #[serde(default = "default_serving_cpich_amplitude")]
    pub control_amplitude: f32,
    #[doc = "Explicit ACK/NACK acceptance thresholds."]
    pub acknowledgement: RustHsdpaEvidence,
    #[doc = "Explicit CQI acceptance thresholds."]
    pub cqi: RustHsdpaEvidence,
}
impl RustHsdpaScheduling {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(1..=65536).contains(&self.maximum_formats) {
            anyhow::bail!("RustHsdpaScheduling.maximum_formats outside 1..=65536");
        }
        if !(1..=1048576).contains(&self.maximum_coded_bits) {
            anyhow::bail!("RustHsdpaScheduling.maximum_coded_bits outside 1..=1048576");
        }
        if !(1..=536870912).contains(&self.maximum_plan_bytes) {
            anyhow::bail!("RustHsdpaScheduling.maximum_plan_bytes outside 1..=536870912");
        }
        if !(1..=536870912).contains(&self.maximum_buffer_bytes) {
            anyhow::bail!("RustHsdpaScheduling.maximum_buffer_bytes outside 1..=536870912");
        }
        if !(1..=536870912).contains(&self.maximum_encoder_bytes) {
            anyhow::bail!("RustHsdpaScheduling.maximum_encoder_bytes outside 1..=536870912");
        }
        if !(1..=1073741824).contains(&self.maximum_ue_bytes) {
            anyhow::bail!("RustHsdpaScheduling.maximum_ue_bytes outside 1..=1073741824");
        }
        if self.maximum_cell_bytes == 0 {
            anyhow::bail!("RustHsdpaScheduling.maximum_cell_bytes must be nonzero");
        }
        if !(1..=4294967295).contains(&self.maximum_cqi_age_chips) {
            anyhow::bail!("RustHsdpaScheduling.maximum_cqi_age_chips outside 1..=4294967295");
        }
        if !(1..=1000000).contains(&self.preparation_guard_us) {
            anyhow::bail!("RustHsdpaScheduling.preparation_guard_us outside 1..=1000000");
        }
        if !(1..=4294967295).contains(&self.selection_budget) {
            anyhow::bail!("RustHsdpaScheduling.selection_budget outside 1..=4294967295");
        }
        if !(0..=10).contains(&self.cqi_offset_limit) {
            anyhow::bail!("RustHsdpaScheduling.cqi_offset_limit outside 0..=10");
        }
        if !(1..=50).contains(&self.cqi_target_bler_percent) {
            anyhow::bail!("RustHsdpaScheduling.cqi_target_bler_percent outside 1..=50");
        }
        if !(self.control_amplitude).is_finite() {
            anyhow::bail!("RustHsdpaScheduling.control_amplitude must be finite");
        }
        if !(0.0..=1.0).contains(&self.control_amplitude) {
            anyhow::bail!("RustHsdpaScheduling.control_amplitude outside 0.0..=1.0");
        }
        (self.acknowledgement).validate_schema()?;
        (self.cqi).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RustHsdpaResources {
    #[doc = "Explicit Rust scheduling resource limits and feedback thresholds. Absence leaves packet scheduling unconfigured. Does not bypass the HSPA serving guard."]
    #[serde(default)]
    pub scheduling: Option<RustHsdpaScheduling>,
    #[doc = "Cell-wide HS-PDSCH SF16 code reservation. Admission also checks common and dedicated code conflicts. Does not enable HSDPA."]
    #[serde(default = "default_serving_hsdpa_data_codes")]
    pub data_codes: u8,
    #[doc = "Cell-wide HS-SCCH SF128 code reservation. UEs share this pool through scheduling."]
    #[serde(default = "default_serving_hsdpa_control_codes")]
    pub control_codes: u8,
    #[doc = "Maximum prepared HSDPA transmissions retained by the shared TX owner."]
    #[serde(default = "default_serving_hsdpa_pending")]
    pub pending_transmissions: u8,
    #[doc = "Cell-wide storage budget for pending HSDPA waveforms and TX bookkeeping, separate from packet queues and decoder memory."]
    #[serde(default = "default_serving_hsdpa_tx_bytes")]
    pub maximum_tx_bytes: u32,
    #[doc = "Retained MAC-d PDUs per HSDPA priority queue."]
    #[serde(default = "default_serving_hsdpa_queue_pdus")]
    pub pdus_per_queue: u16,
    #[doc = "Combined MAC-d queue storage budget per UE, including queue metadata. Does not include HARQ coding or TX waveforms."]
    #[serde(default = "default_serving_hsdpa_ue_queue_bytes")]
    pub maximum_queue_bytes_per_ue: u32,
    #[doc = "Cell-wide MAC-d queue budget. Must cover maximum_ues times maximum_queue_bytes_per_ue."]
    #[serde(default = "default_serving_hsdpa_queue_bytes")]
    pub maximum_queue_bytes: u32,
}
impl Default for RustHsdpaResources {
    fn default() -> Self {
        Self {
            scheduling: Default::default(),
            data_codes: default_serving_hsdpa_data_codes(),
            control_codes: default_serving_hsdpa_control_codes(),
            pending_transmissions: default_serving_hsdpa_pending(),
            maximum_tx_bytes: default_serving_hsdpa_tx_bytes(),
            pdus_per_queue: default_serving_hsdpa_queue_pdus(),
            maximum_queue_bytes_per_ue: default_serving_hsdpa_ue_queue_bytes(),
            maximum_queue_bytes: default_serving_hsdpa_queue_bytes(),
        }
    }
}
impl RustHsdpaResources {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if let Some(value) = &self.scheduling {
            (value).validate_schema()?;
        }
        if !(1..=15).contains(&self.data_codes) {
            anyhow::bail!("RustHsdpaResources.data_codes outside 1..=15");
        }
        if !(1..=15).contains(&self.control_codes) {
            anyhow::bail!("RustHsdpaResources.control_codes outside 1..=15");
        }
        if !(1..=64).contains(&self.pending_transmissions) {
            anyhow::bail!("RustHsdpaResources.pending_transmissions outside 1..=64");
        }
        if !(1..=536870912).contains(&self.maximum_tx_bytes) {
            anyhow::bail!("RustHsdpaResources.maximum_tx_bytes outside 1..=536870912");
        }
        if !(1..=4096).contains(&self.pdus_per_queue) {
            anyhow::bail!("RustHsdpaResources.pdus_per_queue outside 1..=4096");
        }
        if !(1..=536870912).contains(&self.maximum_queue_bytes_per_ue) {
            anyhow::bail!("RustHsdpaResources.maximum_queue_bytes_per_ue outside 1..=536870912");
        }
        if !(1..=536870912).contains(&self.maximum_queue_bytes) {
            anyhow::bail!("RustHsdpaResources.maximum_queue_bytes outside 1..=536870912");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct HspaDownlinkSettings {
    #[doc = "Enable dual-cell HSDPA admission for capable UEs."]
    #[serde(default)]
    pub dual_cell: bool,
    #[doc = "Enable single-carrier 64-QAM and MAC-ehs for capable UEs."]
    #[serde(default = "default_hspa_qam64_enabled")]
    pub qam64_enabled: bool,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub fdpch: bool,
    #[serde(default)]
    pub compact_common_channels: bool,
    #[serde(default = "default_hspa_measurement_power_offset_db")]
    pub measurement_power_offset_db: Option<f64>,
    #[serde(default = "default_hspa_harq_processes")]
    pub harq_processes: u8,
    #[serde(default)]
    pub harq_memory_bits: Vec<u32>,
    #[doc = "Downlink AM RLC PDU size in bits for packet bearers of HSDPA-capable UEs.\nReplaces the packet service's downlink rlc_bits for those UEs."]
    #[serde(default = "default_hspa_rlc_pdu_bits")]
    pub rlc_pdu_bits: u16,
    #[doc = "Largest UE receiving window in PDUs used with rlc_pdu_bits. A smaller packet service window is kept."]
    #[serde(default = "default_hspa_rlc_receive_window")]
    pub rlc_receive_window: u16,
    #[serde(default)]
    pub secondary_scrambling_code: u8,
    #[serde(default)]
    pub all_secondary_power_offset_db: f64,
    #[serde(default)]
    pub feedback: HspaDownlinkFeedbackSettings,
    #[serde(default)]
    pub resources: RustHsdpaResources,
}
impl Default for HspaDownlinkSettings {
    fn default() -> Self {
        Self {
            dual_cell: Default::default(),
            qam64_enabled: true,
            enabled: Default::default(),
            fdpch: Default::default(),
            compact_common_channels: Default::default(),
            measurement_power_offset_db: default_hspa_measurement_power_offset_db(),
            harq_processes: default_hspa_harq_processes(),
            harq_memory_bits: Default::default(),
            rlc_pdu_bits: default_hspa_rlc_pdu_bits(),
            rlc_receive_window: default_hspa_rlc_receive_window(),
            secondary_scrambling_code: Default::default(),
            all_secondary_power_offset_db: Default::default(),
            feedback: Default::default(),
            resources: Default::default(),
        }
    }
}
impl HspaDownlinkSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if let Some(value) = &self.measurement_power_offset_db {
            if !(value).is_finite() {
                anyhow::bail!("HspaDownlinkSettings.measurement_power_offset_db must be finite");
            }
        }
        if !(24..=5000).contains(&self.rlc_pdu_bits) {
            anyhow::bail!("HspaDownlinkSettings.rlc_pdu_bits outside 24..=5000");
        }
        if !(1..=2047).contains(&self.rlc_receive_window) {
            anyhow::bail!("HspaDownlinkSettings.rlc_receive_window outside 1..=2047");
        }
        if !(self.all_secondary_power_offset_db).is_finite() {
            anyhow::bail!("HspaDownlinkSettings.all_secondary_power_offset_db must be finite");
        }
        (self.feedback).validate_schema()?;
        (self.resources).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct EdpdchReference {
    #[doc = "TS 25.321 table-0 E-TFCI used as a complete power reference."]
    pub etfci: u8,
    #[doc = "TS 25.213 Table 1B.1 Delta-E-DPDCH index paired with this E-TFCI."]
    pub power_offset: u8,
}
impl EdpdchReference {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RustHsupaReceive {
    #[doc = "Positive saturation limit for decoder likelihoods."]
    pub likelihood_limit: f64,
    #[doc = "Minimum normalized E-DPCCH codeword correlation."]
    pub minimum_control_correlation: f64,
    #[doc = "Minimum E-DPCCH separation from the next codeword."]
    pub minimum_control_margin: f64,
    #[doc = "Maximum turbo decoder iterations per received transport."]
    pub iterations: u8,
    #[doc = "Maximum transport plan bit budget."]
    pub maximum_format_bits: u32,
    #[doc = "Per-UE E-TFC plan catalog limit."]
    pub maximum_catalog_bytes: u32,
    #[doc = "Per-UE retained sample limit."]
    pub retained_sample_bytes: u32,
    #[doc = "Per-UE HARQ combining memory limit."]
    pub soft_buffer_bytes: u32,
    #[doc = "Per-UE receiver scratch memory limit."]
    pub scratch_bytes: u32,
    #[doc = "Total retained receiver memory limit per UE."]
    pub maximum_receive_bytes: u32,
    #[doc = "Maximum queued receive reports per UE."]
    pub report_capacity: u16,
    #[doc = "Receive-report pool memory limit per UE."]
    pub report_bytes: u32,
    #[doc = "Maximum pending packet-delivery entries per UE."]
    pub packet_capacity: u16,
    #[doc = "Packet reorder and delivery memory limit per UE."]
    pub packet_bytes: u32,
    #[doc = "Maximum packet reorder hold in E-DCH TTIs."]
    pub reorder_hold_ttis: u32,
    #[doc = "Maximum in-flight E-HICH feedback commands per UE."]
    pub feedback_capacity: u8,
    #[doc = "E-HICH complex-chip amplitude."]
    #[serde(default = "default_serving_cpich_amplitude")]
    pub feedback_amplitude: f32,
}
impl RustHsupaReceive {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        if !(self.likelihood_limit).is_finite() {
            anyhow::bail!("RustHsupaReceive.likelihood_limit must be finite");
        }
        if !(self.minimum_control_correlation).is_finite() {
            anyhow::bail!("RustHsupaReceive.minimum_control_correlation must be finite");
        }
        if !(0.0..=1.0).contains(&self.minimum_control_correlation) {
            anyhow::bail!("RustHsupaReceive.minimum_control_correlation outside 0.0..=1.0");
        }
        if !(self.minimum_control_margin).is_finite() {
            anyhow::bail!("RustHsupaReceive.minimum_control_margin must be finite");
        }
        if !(0.0..=2.0).contains(&self.minimum_control_margin) {
            anyhow::bail!("RustHsupaReceive.minimum_control_margin outside 0.0..=2.0");
        }
        if !(1..=32).contains(&self.iterations) {
            anyhow::bail!("RustHsupaReceive.iterations outside 1..=32");
        }
        if !(1..=1048576).contains(&self.maximum_format_bits) {
            anyhow::bail!("RustHsupaReceive.maximum_format_bits outside 1..=1048576");
        }
        if !(1..=536870912).contains(&self.maximum_catalog_bytes) {
            anyhow::bail!("RustHsupaReceive.maximum_catalog_bytes outside 1..=536870912");
        }
        if !(1..=536870912).contains(&self.retained_sample_bytes) {
            anyhow::bail!("RustHsupaReceive.retained_sample_bytes outside 1..=536870912");
        }
        if !(1..=536870912).contains(&self.soft_buffer_bytes) {
            anyhow::bail!("RustHsupaReceive.soft_buffer_bytes outside 1..=536870912");
        }
        if !(1..=536870912).contains(&self.scratch_bytes) {
            anyhow::bail!("RustHsupaReceive.scratch_bytes outside 1..=536870912");
        }
        if !(1..=536870912).contains(&self.maximum_receive_bytes) {
            anyhow::bail!("RustHsupaReceive.maximum_receive_bytes outside 1..=536870912");
        }
        if !(1..=4096).contains(&self.report_capacity) {
            anyhow::bail!("RustHsupaReceive.report_capacity outside 1..=4096");
        }
        if !(1..=536870912).contains(&self.report_bytes) {
            anyhow::bail!("RustHsupaReceive.report_bytes outside 1..=536870912");
        }
        if !(1..=4096).contains(&self.packet_capacity) {
            anyhow::bail!("RustHsupaReceive.packet_capacity outside 1..=4096");
        }
        if !(1..=536870912).contains(&self.packet_bytes) {
            anyhow::bail!("RustHsupaReceive.packet_bytes outside 1..=536870912");
        }
        if !(1..=4294967295).contains(&self.reorder_hold_ttis) {
            anyhow::bail!("RustHsupaReceive.reorder_hold_ttis outside 1..=4294967295");
        }
        if !(1..=64).contains(&self.feedback_capacity) {
            anyhow::bail!("RustHsupaReceive.feedback_capacity outside 1..=64");
        }
        if !(self.feedback_amplitude).is_finite() {
            anyhow::bail!("RustHsupaReceive.feedback_amplitude must be finite");
        }
        if !(0.0..=1.0).contains(&self.feedback_amplitude) {
            anyhow::bail!("RustHsupaReceive.feedback_amplitude outside 0.0..=1.0");
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RustHsupaResources {
    #[doc = "Per-UE receive and delivery budgets."]
    pub receive: RustHsupaReceive,
    #[doc = "Cell-wide admitted receive and packet-memory budget."]
    pub maximum_cell_receive_bytes: u64,
    #[doc = "Maximum queued E-AGCH and E-HICH transmissions."]
    pub pending_transmissions: u16,
    #[doc = "Shared E-AGCH and E-HICH transmitter storage limit."]
    pub maximum_tx_bytes: u32,
    #[doc = "Maximum tracked HSUPA transmitter commands."]
    pub command_capacity: u16,
    #[doc = "Maximum HSUPA sessions visited per controller step."]
    pub service_visits: u16,
    #[doc = "E-AGCH complex-chip amplitude."]
    #[serde(default = "default_serving_cpich_amplitude")]
    pub grant_amplitude: f32,
    #[doc = "Optional interval for repeating the current primary all-process E-AGCH grant while a UE reports fresh nonempty demand. Omission disables periodic refresh. Does not increase the configured grant or reset HARQ data."]
    #[serde(default)]
    pub primary_grant_refresh_ms: Option<u32>,
}
impl RustHsupaResources {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.receive).validate_schema()?;
        if !(1..=17179869184).contains(&self.maximum_cell_receive_bytes) {
            anyhow::bail!("RustHsupaResources.maximum_cell_receive_bytes outside 1..=17179869184");
        }
        if !(1..=256).contains(&self.pending_transmissions) {
            anyhow::bail!("RustHsupaResources.pending_transmissions outside 1..=256");
        }
        if !(1..=536870912).contains(&self.maximum_tx_bytes) {
            anyhow::bail!("RustHsupaResources.maximum_tx_bytes outside 1..=536870912");
        }
        if !(1..=256).contains(&self.command_capacity) {
            anyhow::bail!("RustHsupaResources.command_capacity outside 1..=256");
        }
        if !(1..=64).contains(&self.service_visits) {
            anyhow::bail!("RustHsupaResources.service_visits outside 1..=64");
        }
        if !(self.grant_amplitude).is_finite() {
            anyhow::bail!("RustHsupaResources.grant_amplitude must be finite");
        }
        if !(0.0..=1.0).contains(&self.grant_amplitude) {
            anyhow::bail!("RustHsupaResources.grant_amplitude outside 0.0..=1.0");
        }
        if let Some(value) = &self.primary_grant_refresh_ms {
            if !(20..=120000).contains(value) {
                anyhow::bail!("RustHsupaResources.primary_grant_refresh_ms outside 20..=120000");
            }
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct HspaUplinkSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub scheduled: bool,
    #[serde(default)]
    pub rv_zero_only: bool,
    #[serde(default)]
    pub four_code_uplink: bool,
    #[serde(default = "default_hsupa_max_category")]
    pub max_category: u8,
    #[serde(default)]
    pub initial_serving_grant: Option<u8>,
    #[serde(default = "default_hsupa_serving_grant")]
    pub serving_grant: u8,
    #[serde(default = "default_hsupa_edpcch_power_offset")]
    pub edpcch_power_offset: u8,
    #[serde(default = "default_hsupa_edpdch_references")]
    pub edpdch_references: Vec<EdpdchReference>,
    #[serde(default)]
    pub resources: Option<RustHsupaResources>,
}
impl Default for HspaUplinkSettings {
    fn default() -> Self {
        Self {
            enabled: Default::default(),
            scheduled: default_true(),
            rv_zero_only: Default::default(),
            four_code_uplink: Default::default(),
            max_category: default_hsupa_max_category(),
            initial_serving_grant: Default::default(),
            serving_grant: default_hsupa_serving_grant(),
            edpcch_power_offset: default_hsupa_edpcch_power_offset(),
            edpdch_references: default_hsupa_edpdch_references(),
            resources: Default::default(),
        }
    }
}
impl HspaUplinkSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        for value in &self.edpdch_references {
            (value).validate_schema()?;
        }
        if let Some(value) = &self.resources {
            (value).validate_schema()?;
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct HspaSettings {
    #[serde(default)]
    pub downlink: HspaDownlinkSettings,
    #[serde(default)]
    pub uplink: HspaUplinkSettings,
}
impl HspaSettings {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.downlink).validate_schema()?;
        (self.uplink).validate_schema()?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct RustSecurityPolicy {
    #[doc = "Allows unencrypted operation when requested by the core."]
    #[serde(default = "default_security_algorithm_enabled")]
    pub allow_uea0: bool,
    #[doc = "Allows KASUMI ciphering."]
    #[serde(default = "default_security_algorithm_enabled")]
    pub allow_uea1: bool,
    #[doc = "Allows SNOW-3G ciphering."]
    #[serde(default = "default_security_algorithm_enabled")]
    pub allow_uea2: bool,
    #[doc = "Allows KASUMI integrity protection."]
    #[serde(default = "default_security_algorithm_enabled")]
    pub allow_uia1: bool,
    #[doc = "Allows SNOW-3G integrity protection."]
    #[serde(default = "default_security_algorithm_enabled")]
    pub allow_uia2: bool,
}
impl Default for RustSecurityPolicy {
    fn default() -> Self {
        Self {
            allow_uea0: default_security_algorithm_enabled(),
            allow_uea1: default_security_algorithm_enabled(),
            allow_uea2: default_security_algorithm_enabled(),
            allow_uia1: default_security_algorithm_enabled(),
            allow_uia2: default_security_algorithm_enabled(),
        }
    }
}
impl RustSecurityPolicy {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub cell: CellSettings,
    pub core: CoreSettings,
    #[serde(default)]
    pub rel99: Rel99Settings,
    #[serde(default)]
    pub rrc: RrcConfig,
    #[serde(default)]
    pub diagnostics: DiagnosticsSettings,
    #[serde(default)]
    pub management: ManagementSettings,
    #[serde(default)]
    pub runtime: RuntimeSettings,
    pub radio: RadioSettings,
    #[serde(default)]
    pub receiver: ReceiverSettings,
    #[serde(default)]
    pub power_control: PowerControlSettings,
    #[serde(default)]
    pub hspa: HspaSettings,
    #[serde(default)]
    pub security: RustSecurityPolicy,
}
impl Config {
    pub fn validate_schema(&self) -> anyhow::Result<()> {
        (self.cell).validate_schema()?;
        (self.core).validate_schema()?;
        (self.rel99).validate_schema()?;
        (self.rrc).validate_schema()?;
        (self.diagnostics).validate_schema()?;
        (self.management).validate_schema()?;
        (self.runtime).validate_schema()?;
        (self.radio).validate_schema()?;
        (self.receiver).validate_schema()?;
        (self.power_control).validate_schema()?;
        (self.hspa).validate_schema()?;
        (self.security).validate_schema()?;
        Ok(())
    }
}

fn default_hspa_qam64_enabled() -> bool {
    true
}
