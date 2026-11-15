// SPDX-License-Identifier: AGPL-3.0-or-later

scalars!(bool, u8, u16, u32, u64, i8, i16, i32, i64, R99ErrorDelivery, R99TrafficClass, HnbgwTransport, RustCircuitInitialization, R99CodingKind, R99TpcAlgorithm, SimdMode, RrcReleaseTransportPolicy);

scalars!(PacketMode, Rel99Profile, NetworkOperationMode);

records! {
    R99ErrorRatio {
        mantissa: u8,
        exponent: u8,
    }
    R99Reliability {
        residual: R99ErrorRatio,
        sdu: Option<R99ErrorRatio>,
        delivery: R99ErrorDelivery,
    }
    R99ServiceContract {
        traffic_class: R99TrafficClass,
        delivery_order: bool,
        maximum_transfer_delay_ms: Option<u16>,
        subflows: Vec<R99Reliability>,
    }
    R99CircuitSubflow {
        uplink: R99Coding,
        downlink: R99Coding,
        empty_block: bool,
    }
    R99CircuitFormat {
        subflow_bits: Vec<u16>,
    }
    R99CircuitService {
        name: String,
        contract: R99ServiceContract,
        interval_ms: u8,
        subflows: Vec<R99CircuitSubflow>,
        formats: Vec<R99CircuitFormat>,
        versions: u16,
    }
    R99PacketChannel {
        interval_ms: u8,
        rlc_bits: u16,
        payload_budget_bits: u16,
        maximum_blocks: u16,
        coding: R99Coding,
    }
    R99BufferLimits {
        maximum_sdu_bytes: u32,
        queued_sdus: u16,
        maximum_buffer_bytes: u32,
    }
    R99AmPeer {
        uplink_window: u16,
        downlink_window: u16,
        maximum_data_attempts: u16,
        maximum_reset_attempts: u16,
        reset_timeout_ms: u16,
        poll_timeout_ms: Option<u16>,
        poll_prohibit_ms: Option<u16>,
        poll_every_pdus: Option<u16>,
        last_transmission: bool,
        last_retransmission: bool,
        status_prohibit_ms: Option<u16>,
        missing_pdu_indicator: bool,
        in_sequence_delivery: bool,
    }
    R99NetworkAmPolicy {
        maximum_data_attempts: u16,
        maximum_reset_attempts: u16,
        reset_timeout_ms: u16,
        poll_timeout_ms: Option<u16>,
        poll_prohibit_ms: Option<u16>,
        poll_every_pdus: Option<u16>,
        last_transmission: bool,
        last_retransmission: bool,
        status_prohibit_ms: Option<u16>,
        missing_pdu_indicator: bool,
    }
    R99AmPolicy {
        ue: R99AmPeer,
        network: R99NetworkAmPolicy,
        buffers: R99BufferLimits,
    }
    R99UmPolicy {
        uplink: R99BufferLimits,
        downlink: R99BufferLimits,
    }
    R99PacketService {
        name: String,
        contract: R99ServiceContract,
        uplink: R99PacketChannel,
        downlink: R99PacketChannel,
        am: Option<R99AmPolicy>,
        um: Option<R99UmPolicy>,
    }
    EdpdchReference {
        etfci: u8,
        power_offset: u8,
    }
    RustHsdpaEvidence {
        minimum_correlation: f64,
        minimum_margin: f64,
        minimum_pilot_fraction: f64,
        maximum_relative_disturbance: f64,
        minimum_relative_soft_power: f64,
    }
    RustHsdpaScheduling {
        maximum_formats: u32,
        maximum_coded_bits: u32,
        maximum_plan_bytes: u32,
        maximum_buffer_bytes: u32,
        maximum_encoder_bytes: u32,
        maximum_ue_bytes: u32,
        maximum_cell_bytes: u32,
        maximum_cqi_age_chips: u32,
        preparation_guard_us: u32,
        selection_budget: u32,
        cqi_offset_limit: u8,
        cqi_target_bler_percent: u8,
        control_amplitude: f32,
        acknowledgement: RustHsdpaEvidence,
        cqi: RustHsdpaEvidence,
    }
    RustHsupaReceive {
        likelihood_limit: f64,
        minimum_control_correlation: f64,
        minimum_control_margin: f64,
        iterations: u8,
        maximum_format_bits: u32,
        maximum_catalog_bytes: u32,
        retained_sample_bytes: u32,
        soft_buffer_bytes: u32,
        scratch_bytes: u32,
        maximum_receive_bytes: u32,
        report_capacity: u16,
        report_bytes: u32,
        packet_capacity: u16,
        packet_bytes: u32,
        reorder_hold_ttis: u32,
        feedback_capacity: u8,
        feedback_amplitude: f32,
    }
    RustHsupaResources {
        receive: RustHsupaReceive,
        maximum_cell_receive_bytes: u64,
        pending_transmissions: u16,
        maximum_tx_bytes: u32,
        command_capacity: u16,
        service_visits: u16,
        grant_amplitude: f32,
        primary_grant_refresh_ms: Option<u32>,
    }
    RustDownlinkPowerConfig {
        crest_reduction_enabled: bool,
        carrier_rms: f32,
        peak_margin_db: f32,
        cpich_percent: f32,
        broadcast_percent: f32,
        broadcast_sync_percent: f32,
        fach_percent: f32,
        pch_percent: f32,
        pich_percent: f32,
        aich_percent: f32,
        dedicated_percent: f32,
        hsdpa_percent: f32,
        hs_scch_percent: f32,
        e_agch_percent: f32,
        e_hich_percent: f32,
    }
    R99Coding {
        kind: R99CodingKind,
        crc_bits: u8,
        rate_matching_attribute: u16,
    }
    R99PhysicalCandidate {
        uplink_minimum_sf: u16,
        uplink_dpdch_count: u8,
        uplink_tfci: bool,
        dpcch_offset_db: i16,
        pc_preamble_frames: u8,
        srb_delay_frames: u8,
        tpc_algorithm: R99TpcAlgorithm,
        tpc_step_db: u8,
        downlink_sf: u16,
        downlink_pilot_bits: u8,
        downlink_tfci: bool,
        downlink_code_count: u8,
        flexible_position: bool,
        pilot_offset_quarter_db: u8,
        default_offset_512_chips: u16,
    }
}

properties! {
    "rel99.profile": Option<Rel99Profile>, false;
    "cell.packet_modes": Vec<PacketMode>, false;
    "cell.secondary_carrier.enabled": bool, false;
    "cell.secondary_carrier.uarfcn_dl": Option<u32>, false;
    "cell.secondary_carrier.primary_scrambling_code": Option<u16>, false;
    "cell.secondary_carrier.power_percent": f64, false;
    "cell.plmn": String, true;
    "cell.lac": u16, true;
    "cell.rac": u8, true;
    "cell.sac": u16, true;
    "cell.cell_id": u32, true;
    "cell.hnb_identity": String, true;
    "cell.gprs_enable": bool, false;
    "cell.nmo": NetworkOperationMode, false;
    "cell.q_qual_min": i8, false;
    "cell.physical.downlink_scrambling_code": u16, true;
    "cell.physical.dch_secondary_scrambling_code": u8, false;
    "cell.access.prach.initial_power_constant_db": i8, false;
    "cell.access.prach.signature": u8, false;
    "cell.access.prach.subchannel": u8, false;
    "cell.access.prach.scrambling_code_word_number": u8, false;
    "cell.access.prach.dynamic_persistence_level": u8, false;
    "cell.common_channels.aich_power_offset_db": i8, false;
    "cell.measurements.request_cpich_measurements": bool, false;
    "cell.measurements.ue_measurement_report_count": Option<u8>, false;
    "cell.measurements.ue_measurement_interval_ms": u32, false;
    "cell.broadcast_state_file": PathBuf, true;
    "cell.advertised_power.cpich_tx_dbm": Option<i8>, true;
    "cell.advertised_power.uplink_interference_dbm": Option<i16>, true;
    "core.hnbgw.address": String, true;
    "core.hnbgw.port": u16, true;
    "core.hnbgw.transport": HnbgwTransport, true;
    "core.hnbgw.udp_encapsulation_port": Option<u16>, false;
    "core.hnbgw.udp_encapsulation_local_port": Option<u16>, false;
    "core.gtpu.bind_ip": String, true;
    "core.gtpu.bind_port": u16, false;
    "core.gtpu.advertise_ip": Option<String>, false;
    "core.gtpu.allow_ephemeral_port": bool, false;
    "core.gtpu.release_rab_on_error_indication": bool, false;
    "core.iuh.retry_attempts": u32, false;
    "core.iuh.retry_backoff_secs": u64, false;
    "core.hnbap_tmsi_registration": bool, false;
    "rel99.physical.planner.services.circuit": Vec<R99CircuitService>, false;
    "rel99.physical.planner.services.packet": Vec<R99PacketService>, false;
    "rel99.physical.planner.signalling.uplink.kind": R99CodingKind, false;
    "rel99.physical.planner.signalling.uplink.crc_bits": u8, false;
    "rel99.physical.planner.signalling.uplink.rate_matching_attribute": u16, false;
    "rel99.physical.planner.signalling.downlink.kind": R99CodingKind, false;
    "rel99.physical.planner.signalling.downlink.crc_bits": u8, false;
    "rel99.physical.planner.signalling.downlink.rate_matching_attribute": u16, false;
    "rel99.physical.planner.signalling.interval_ms": u8, false;
    "rel99.physical.planner.signalling.maximum_blocks": u16, false;
    "rel99.physical.planner.signalling.um_uplink_bits": u16, false;
    "rel99.physical.planner.signalling.um_downlink_bits": u16, false;
    "rel99.physical.planner.physical_candidates": Vec<R99PhysicalCandidate>, false;
    "rel99.physical.planner.budgets.maximum_combinations": u16, false;
    "rel99.physical.planner.budgets.maximum_total_coded_bits": u32, false;
    "rel99.physical.planner.budgets.maximum_user_storage_bytes": u32, false;
    "rel99.physical.planner.budgets.maximum_transport_coded_bits": u32, false;
    "rel99.physical.planner.budgets.maximum_mapping_entries": u32, false;
    "rel99.physical.planner.uplink_control_gain": u8, false;
    "rel99.physical.planner.uplink_data_gain": u8, false;
    "rel99.physical.initial_ul_puncturing_limit_percent": u8, false;
    "rel99.physical.uplink_scrambling_code_base": u32, false;
    "rrc.release_transport_policy": RrcReleaseTransportPolicy, false;
    "rrc.srnc_id": u16, false;
    "rrc.ura_identity": u16, false;
    "rrc.cn_drx_cycle_length_coefficient": u8, false;
    "rrc.periodic_lu_timer_value": u8, false;
    "rrc.max_allowed_ul_tx_power_dbm": i8, false;
    "rrc.inactivity_release_seconds": u32, false;
    "rrc.release_complete_retransmissions": u8, false;
    "rrc.inactivity_delete_seconds": u32, false;
    "rrc.combined_bearer_setup": bool, false;
    "diagnostics.logging.filter": String, false;
    "management.grpc.enabled": bool, false;
    "management.grpc.socket_path": PathBuf, false;
    "management.grpc.administrator_socket_path": PathBuf, false;
    "management.grpc.tcp_address": Option<String>, false;
    "management.grpc.tls_certificate": Option<PathBuf>, false;
    "management.grpc.tls_key": Option<PathBuf>, false;
    "management.grpc.tls_client_ca": Option<PathBuf>, false;
    "management.grpc.administrator_certificates_sha256": Vec<String>, false;
    "management.grpc.capture_directory": PathBuf, false;
    "management.grpc.maximum_capture_disk_mib": u32, false;
    "runtime.dch_cpu": Option<u32>, false;
    "runtime.scheduling.enabled": bool, false;
    "runtime.scheduling.main_cpu": Option<u32>, false;
    "runtime.scheduling.control_cpu": Option<u32>, false;
    "runtime.scheduling.usb_cpu": Option<u32>, false;
    "runtime.scheduling.prepare_cpu": Option<u32>, false;
    "runtime.scheduling.submit_cpu": Option<u32>, false;
    "runtime.scheduling.receive_cpu": Option<u32>, false;
    "runtime.scheduling.reader_cpu": Option<u32>, false;
    "runtime.scheduling.usb_priority": u8, false;
    "runtime.scheduling.submit_priority": u8, false;
    "runtime.scheduling.reader_priority": u8, false;
    "runtime.scheduling.prepare_priority": u8, false;
    "runtime.scheduling.receive_priority": u8, false;
    "runtime.simd": SimdMode, false;
    "runtime.media.maximum_packet_bytes": u32, false;
    "runtime.media.maximum_voice_bytes": u32, false;
    "runtime.media.circuit.bind_ip": Option<String>, false;
    "runtime.media.circuit.advertise_ip": Option<String>, false;
    "runtime.media.circuit.payload_type": u8, false;
    "runtime.media.circuit.datagram_bytes": u16, false;
    "runtime.media.circuit.wait_ms": u32, false;
    "runtime.media.circuit.playout_lead_ms": u32, false;
    "runtime.media.circuit.horizon_ms": u32, false;
    "runtime.media.circuit.initialization": RustCircuitInitialization, false;
    "runtime.media.circuit.versions": u16, false;
    "runtime.media.circuit.timeout_ms": u32, false;
    "runtime.media.circuit.repetitions": u8, false;
    "runtime.clock.interval_us": u32, false;
    "runtime.clock.maximum_age_us": Option<u32>, false;
    "runtime.clock.maximum_query_us": u32, false;
    "runtime.clock.maximum_drift_ppm": u32, false;
    "runtime.recovery.maximum_restarts": u16, false;
    "runtime.recovery.backoff_ms": u32, false;
    "runtime.recovery.maximum_backoff_ms": u32, false;
    "runtime.spin_wait": bool, false;
    "runtime.lock_memory": bool, false;
    "runtime.tx_batch_slots": u8, false;
    "runtime.maximum_ues": u32, true;
    "runtime.maximum_sdu_bytes": u32, false;
    "runtime.queued_sdus_per_bearer": u32, false;
    "runtime.maximum_queue_bytes": u32, false;
    "runtime.rx_timeout_us": u32, false;
    "runtime.rx_queue_blocks": u16, false;
    "runtime.startup_lead_ms": u32, false;
    "runtime.minimum_submission_us": u32, false;
    "runtime.maximum_submission_us": u32, false;
    "runtime.maximum_preparation_us": u32, false;
    "runtime.idle_wait_us": u32, false;
    "runtime.controller_idle_wait_us": Option<u32>, false;
    "runtime.startup_timeout_ms": u32, false;
    "runtime.clock_stall_timeout_ms": u32, false;
    "runtime.join_timeout_ms": u32, false;
    "radio.backend": String, true;
    "radio.soapy_args": String, false;
    "radio.uhd_args": String, true;
    "radio.tx_antenna": Option<String>, false;
    "radio.rx_antenna": Option<String>, false;
    "radio.uhd_rx_timestamp_offset": Option<i64>, false;
    "radio.bladerf_args": String, false;
    "radio.bladerf_rx_sample_phase": u8, false;
    "radio.uarfcn_dl": u32, true;
    "radio.uarfcn_ul": u32, true;
    "radio.tx_gain_db": f64, true;
    "radio.rx_gain_db": f64, true;
    "radio.rx_delay_samples": f64, true;
    "radio.tx_samples_per_chip": u8, false;
    "radio.tx_analog_bandwidth_hz": u32, false;
    "radio.tx_lo_offset_hz": i64, false;
    "radio.bladerf_stream_buffers": u32, false;
    "radio.bladerf_stream_buffer_samples": u32, false;
    "radio.bladerf_stream_transfers": u32, false;
    "radio.bladerf_rx_timestamp_offset": Option<i64>, false;
    "receiver.prach.minimum_peak_to_background_ratio": f64, false;
    "receiver.turbo_iterations": u32, false;
    "receiver.filter_span_chips": u16, false;
    "receiver.dedicated.maximum_shared_bytes": u32, false;
    "receiver.dedicated.maximum_sample_bytes": u32, false;
    "receiver.dedicated.maximum_coded_bits": u32, false;
    "receiver.dedicated.maximum_output_bytes": u32, false;
    "receiver.dedicated.maximum_demultiplexed_bits": u32, false;
    "receiver.dedicated.maximum_bearer_bytes": u32, false;
    "receiver.dedicated.maximum_transmit_bits": u32, false;
    "receiver.dedicated.maximum_frame_bits": u32, false;
    "receiver.dedicated.history_frames": u8, false;
    "receiver.dedicated.work_per_cycle": u16, false;
    "receiver.dedicated.command_capacity": u16, false;
    "receiver.dedicated.output_slots": u16, false;
    "receiver.dedicated.search_radius_samples": u16, false;
    "receiver.dedicated.minimum_coherent_fraction": f64, false;
    "receiver.dedicated.carrier_initial_hz": f64, false;
    "receiver.dedicated.carrier_maximum_absolute_hz": f64, false;
    "receiver.dedicated.carrier_update_fraction": f64, false;
    "receiver.dedicated.carrier_minimum_coherence": f64, false;
    "receiver.dedicated.likelihood_maximum": f64, false;
    "receiver.dedicated.tfci_minimum_score": f64, false;
    "receiver.dedicated.tfci_minimum_margin": f64, false;
    "receiver.dedicated.radio_link_out_of_sync_frames": u32, false;
    "receiver.dedicated.radio_link_in_sync_frames": u32, false;
    "receiver.dedicated.radio_link_failure_frames": u32, false;
    "power_control.uplink.sir_ceiling_db": f64, false;
    "power_control.uplink.initial_sir_db": Option<f64>, false;
    "power_control.uplink.sir_floor_db": Option<f64>, false;
    "power_control.uplink.target_bler": f64, false;
    "power_control.uplink.tpc_hold_groups": u8, false;
    "power_control.downlink.pich_power_offset_db": i8, false;
    "power_control.downlink.allocation": Option<RustDownlinkPowerConfig>, false;
    "power_control.downlink.cpich_amplitude": f32, false;
    "power_control.downlink.common_channel_offset_db": f32, false;
    "power_control.downlink.dedicated.downlink_offset_db": f32, false;
    "power_control.downlink.dedicated.downlink_minimum_offset_db": f32, false;
    "power_control.downlink.dedicated.tpc_offset_db": f32, false;
    "power_control.downlink.dedicated.tpc_maximum_observation_age_chips": u32, false;
    "power_control.downlink.dedicated.tpc_initial_maximum_slots": u32, false;
    "hspa.downlink.qam64_enabled": bool, false;
    "hspa.downlink.dual_cell": bool, false;
    "hspa.downlink.enabled": bool, false;
    "hspa.downlink.fdpch": bool, false;
    "hspa.downlink.compact_common_channels": bool, false;
    "hspa.downlink.measurement_power_offset_db": Option<f64>, false;
    "hspa.downlink.harq_processes": u8, false;
    "hspa.downlink.harq_memory_bits": Vec<u32>, false;
    "hspa.downlink.rlc_pdu_bits": u16, false;
    "hspa.downlink.rlc_receive_window": u16, false;
    "hspa.downlink.secondary_scrambling_code": u8, false;
    "hspa.downlink.all_secondary_power_offset_db": f64, false;
    "hspa.downlink.feedback.ack_nack_power_index": u8, false;
    "hspa.downlink.feedback.cqi_power_index": u8, false;
    "hspa.downlink.feedback.cqi_interval_ms": u16, false;
    "hspa.downlink.resources.scheduling": Option<RustHsdpaScheduling>, false;
    "hspa.downlink.resources.data_codes": u8, false;
    "hspa.downlink.resources.control_codes": u8, false;
    "hspa.downlink.resources.pending_transmissions": u8, false;
    "hspa.downlink.resources.maximum_tx_bytes": u32, false;
    "hspa.downlink.resources.pdus_per_queue": u16, false;
    "hspa.downlink.resources.maximum_queue_bytes_per_ue": u32, false;
    "hspa.downlink.resources.maximum_queue_bytes": u32, false;
    "hspa.uplink.enabled": bool, false;
    "hspa.uplink.scheduled": bool, false;
    "hspa.uplink.rv_zero_only": bool, false;
    "hspa.uplink.four_code_uplink": bool, false;
    "hspa.uplink.max_category": u8, false;
    "hspa.uplink.initial_serving_grant": Option<u8>, false;
    "hspa.uplink.serving_grant": u8, false;
    "hspa.uplink.edpcch_power_offset": u8, false;
    "hspa.uplink.edpdch_references": Vec<EdpdchReference>, false;
    "hspa.uplink.resources": Option<RustHsupaResources>, false;
    "security.allow_uea0": bool, false;
    "security.allow_uea1": bool, false;
    "security.allow_uea2": bool, false;
    "security.allow_uia1": bool, false;
    "security.allow_uia2": bool, false;
}
