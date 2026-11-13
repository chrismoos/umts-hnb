// SPDX-License-Identifier: AGPL-3.0-or-later

use std::net::Ipv4Addr;

use anyhow::{Context, Result, anyhow, bail};
mod deployment;
pub use deployment::Deployment;
mod network_mode;
#[allow(clippy::collapsible_if)]
mod settings;
pub use network_mode::NetworkOperationMode;
mod packet_modes;
mod paths;
mod profiles;
mod receiver;
mod rel99;
pub use profiles::Rel99Profile;
pub use receiver::PrachReceiverSettings;
mod secondary_carrier;
pub use packet_modes::PacketMode;
pub use settings::*;

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let explicit: serde_yaml::Value =
            serde_yaml::from_str(&text).context("parsing YAML config")?;
        let mut resolved = explicit.clone();
        profiles::resolve(&mut resolved)?;
        let mut cfg: Config =
            serde_yaml::from_value(resolved).context("parsing resolved config")?;
        cfg.resolve_packet_modes(&explicit)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_schema()?;
        self.validate_packet_modes()?;
        self.validate_secondary_carrier()?;
        if let Some(count) = self.cell.measurements.ue_measurement_report_count
            && !matches!(count, 1 | 2 | 4 | 8 | 16 | 32 | 64)
        {
            bail!("ue_measurement_report_count must be 1, 2, 4, 8, 16, 32, 64 or null");
        }
        if !matches!(
            self.cell.measurements.ue_measurement_interval_ms,
            250 | 500
                | 1000
                | 2000
                | 3000
                | 4000
                | 6000
                | 8000
                | 12000
                | 16000
                | 20000
                | 24000
                | 28000
                | 32000
                | 64000
        ) {
            bail!("ue_measurement_interval_ms must be a supported TS 25.331 reporting interval");
        }
        let security = &self.security;
        if !security.allow_uia1 && !security.allow_uia2 {
            bail!("security must allow an integrity algorithm");
        }
        if !security.allow_uea0 && !security.allow_uea1 && !security.allow_uea2 {
            bail!("security must allow a ciphering algorithm");
        }
        self.management.grpc.validate_policy()?;
        self.rel99.physical.planner.validate_policy()?;
        let _ = parse_plmn(&self.cell.plmn)?;
        if self.cell.lac == 0 {
            bail!("cell.lac must be in 1..=65535 (RFU/reserved 0)");
        }
        if self.cell.rac == 0 {
            bail!("cell.rac must be in 1..=255");
        }
        if self.cell.cell_id >= (1 << 28) {
            bail!(
                "cell.cell_id must fit in 28 bits (got 0x{:x})",
                self.cell.cell_id
            );
        }

        if self.cell.physical.downlink_scrambling_code > 511 {
            bail!("cell.physical.downlink_scrambling_code must be in 0..=511");
        }
        if self.radio.bladerf_rx_sample_phase > 1 {
            bail!("radio.bladerf_rx_sample_phase must be 0 or 1");
        }
        if self.hspa.downlink.secondary_scrambling_code > 15 {
            return Err(anyhow::anyhow!(
                "hspa.downlink.secondary_scrambling_code must be in 0..=15"
            ));
        }
        if self.hspa.downlink.fdpch
            && (!self.hspa.downlink.enabled
                || (self.hspa.downlink.secondary_scrambling_code != 0
                    && self.hspa.downlink.secondary_scrambling_code
                        != self.cell.physical.dch_secondary_scrambling_code)
                || (!self.hspa.uplink.enabled
                    && (self.hspa.downlink.secondary_scrambling_code != 0
                        || self.cell.physical.dch_secondary_scrambling_code != 0)))
        {
            return Err(anyhow::anyhow!(
                "hspa.downlink.fdpch requires HSDPA-only on primary, or combined HSUPA using all-primary, primary-HS/secondary-control, or one equal secondary scrambling code for the complete HSPA tree"
            ));
        }
        if self.hspa.downlink.compact_common_channels
            && (!self.hspa.downlink.enabled
                || self.hspa.downlink.secondary_scrambling_code != 0
                || self.cell.physical.dch_secondary_scrambling_code != 0)
        {
            bail!(
                "hspa.downlink.compact_common_channels requires primary HSPA and primary dedicated channels"
            );
        }
        if self
            .hspa
            .downlink
            .measurement_power_offset_db
            .is_some_and(|gamma| {
                !gamma.is_finite()
                    || !(-6.0..=13.0).contains(&gamma)
                    || ((gamma * 2.0).round() - gamma * 2.0).abs() > 1e-9
            })
        {
            return Err(anyhow::anyhow!(
                "hspa.downlink.measurement_power_offset_db must be -6..=13 in 0.5 dB steps"
            ));
        }
        let secondary_power = self.hspa.downlink.all_secondary_power_offset_db;
        if !secondary_power.is_finite()
            || !(-12.0..=12.0).contains(&secondary_power)
            || ((secondary_power * 2.0).round() - secondary_power * 2.0).abs() > 1e-9
            || (secondary_power != 0.0
                && (self.hspa.downlink.secondary_scrambling_code == 0
                    || self.hspa.downlink.secondary_scrambling_code
                        != self.cell.physical.dch_secondary_scrambling_code))
        {
            return Err(anyhow::anyhow!(
                "hspa.downlink.all_secondary_power_offset_db must be -12..=12 in 0.5 dB steps and non-zero only for one equal all-secondary HSPA tree"
            ));
        }
        if self.hspa.downlink.feedback.ack_nack_power_index > 8 {
            return Err(anyhow::anyhow!(
                "hspa.downlink.feedback.ack_nack_power_index must be in 0..=8"
            ));
        }
        let uplink_target = self.power_control.uplink.sir_ceiling_db;
        if !matches!(
            self.hspa.downlink.feedback.cqi_interval_ms,
            2 | 4 | 8 | 10 | 20 | 40 | 80 | 160
        ) {
            anyhow::bail!(
                "hspa.downlink.feedback.cqi_interval_ms must be 2, 4, 8, 10, 20, 40, 80 or 160"
            );
        }
        if !uplink_target.is_finite() || !(0.0..=45.0).contains(&uplink_target) {
            return Err(anyhow::anyhow!(
                "power_control.uplink.sir_ceiling_db must be finite and in 0..=45 dB"
            ));
        }
        if let Some(floor) = self.power_control.uplink.sir_floor_db
            && (!floor.is_finite() || !(0.0..=uplink_target).contains(&floor))
        {
            return Err(anyhow::anyhow!(
                "power_control.uplink.sir_floor_db must be finite and in 0..=hspa_uplink_dpcch_sir_target_db"
            ));
        }
        if let Some(initial) = self.power_control.uplink.initial_sir_db {
            let floor = self
                .power_control
                .uplink
                .sir_floor_db
                .unwrap_or(uplink_target);
            if !initial.is_finite() || !(floor..=uplink_target).contains(&initial) {
                return Err(anyhow::anyhow!(
                    "power_control.uplink.initial_sir_db must lie between the uplink SIR floor and ceiling"
                ));
            }
        }
        let uplink_bler = self.power_control.uplink.target_bler;
        if !uplink_bler.is_finite() || !(0.01..=0.50).contains(&uplink_bler) {
            return Err(anyhow::anyhow!(
                "power_control.uplink.target_bler must be finite and in 0.01..=0.50"
            ));
        }
        if self.power_control.uplink.tpc_hold_groups > 8 {
            return Err(anyhow::anyhow!(
                "power_control.uplink.tpc_hold_groups must be in 0..=8"
            ));
        }

        if !(1..=8).contains(&self.hspa.downlink.harq_processes) {
            return Err(anyhow::anyhow!(
                "hspa.downlink.harq_processes must be in 1..=8"
            ));
        }

        if !self.hspa.downlink.rlc_pdu_bits.is_multiple_of(8) {
            return Err(anyhow::anyhow!(
                "hspa.downlink.rlc_pdu_bits must be a whole number of octets"
            ));
        }

        if !self.hspa.downlink.harq_memory_bits.is_empty() {
            if self.hspa.downlink.harq_memory_bits.len()
                != self.hspa.downlink.harq_processes as usize
            {
                return Err(anyhow::anyhow!(
                    "hspa.downlink.harq_memory_bits must contain one value per HARQ process"
                ));
            }
            const ALLOWED: &[u32] = &[
                800, 1600, 2400, 3200, 4000, 4800, 5600, 6400, 7200, 8000, 8800, 9600, 10400,
                11200, 12000, 12800, 13600, 14400, 15200, 16000, 17600, 19200, 20800, 22400, 24000,
                25600, 27200, 28800, 30400, 32000, 36000, 40000, 44000, 48000, 52000, 56000, 60000,
                64000, 68000, 72000, 76000, 80000, 88000, 96000, 104000, 112000, 120000, 128000,
                136000, 144000, 152000, 160000, 176000, 192000, 208000, 224000, 240000, 256000,
                272000, 288000, 304000,
            ];
            if self
                .hspa
                .downlink
                .harq_memory_bits
                .iter()
                .any(|v| !ALLOWED.contains(v))
            {
                return Err(anyhow::anyhow!(
                    "hspa.downlink.harq_memory_bits contains a value not enumerated by TS 25.331"
                ));
            }
        }
        if !(1..=6).contains(&self.hspa.uplink.max_category) {
            return Err(anyhow::anyhow!("hspa.uplink.max_category must be in 1..=6"));
        }
        if self.hspa.uplink.four_code_uplink
            && (!self.hspa.downlink.enabled
                || !self.hspa.uplink.enabled
                || !self.hspa.uplink.scheduled
                || self.hspa.uplink.max_category < 6)
        {
            return Err(anyhow::anyhow!(
                "hspa.uplink.four_code_uplink requires hspa_enable, hsupa_enable, hsupa_scheduled, and hsupa_max_category=6"
            ));
        }
        if !(2..=31).contains(&self.hspa.uplink.serving_grant) {
            return Err(anyhow::anyhow!(
                "hspa.uplink.serving_grant must be in 2..=31"
            ));
        }
        if let Some(initial) = self.hspa.uplink.initial_serving_grant
            && !(2..=self.hspa.uplink.serving_grant).contains(&initial)
        {
            return Err(anyhow::anyhow!(
                "hspa.uplink.initial_serving_grant must be in 2..=hsupa_serving_grant"
            ));
        }

        if self.hspa.uplink.edpcch_power_offset > 8 {
            return Err(anyhow::anyhow!(
                "hspa.uplink.edpcch_power_offset must be in 0..=8"
            ));
        }
        if self.hspa.uplink.edpdch_references.is_empty()
            || self.hspa.uplink.edpdch_references.len() > 8
        {
            return Err(anyhow::anyhow!(
                "hspa.uplink.edpdch_references must contain 1..=8 complete reference points"
            ));
        }
        let mut previous = None;
        for reference in &self.hspa.uplink.edpdch_references {
            if reference.power_offset > 29 {
                return Err(anyhow::anyhow!(
                    "hspa.uplink.edpdch_references power_offset must be in 0..=29"
                ));
            }
            if previous.is_some_and(|value| reference.etfci <= value) {
                return Err(anyhow::anyhow!(
                    "hspa.uplink.edpdch_references must be in strictly increasing E-TFCI order"
                ));
            }
            // TS 25.331 8.6.6.37 explicitly leaves table-0/2-ms E-TFCI 120
            if self.hspa.uplink.max_category.is_multiple_of(2) && reference.etfci == 120 {
                return Err(anyhow::anyhow!(
                    "hspa.uplink.edpdch_references cannot use table-0 E-TFCI 120 with a 2-ms category"
                ));
            }
            previous = Some(reference.etfci);
        }
        if self.cell.physical.dch_secondary_scrambling_code > 15 {
            bail!("cell.physical.dch_secondary_scrambling_code must be in 0..=15");
        }
        if self.cell.physical.dch_secondary_scrambling_code != 0
            && self.hspa.downlink.secondary_scrambling_code != 0
            && (self.cell.physical.dch_secondary_scrambling_code
                != self.hspa.downlink.secondary_scrambling_code
                || !self.hspa.downlink.fdpch
                || !self.hspa.uplink.enabled)
        {
            bail!(
                "non-zero cell.physical.dch_secondary_scrambling_code and hspa.downlink.secondary_scrambling_code must be equal and require the combined F-DPCH/HSUPA all-secondary construction"
            );
        }
        if !(4..=64).contains(&self.radio.bladerf_stream_buffers) {
            bail!("radio.bladerf_stream_buffers must be in 4..=64");
        }
        if !(1024..=32768).contains(&self.radio.bladerf_stream_buffer_samples)
            || !self
                .radio
                .bladerf_stream_buffer_samples
                .is_multiple_of(1024)
        {
            bail!(
                "radio.bladerf_stream_buffer_samples must be in 1024..=32768 and a multiple of 1024"
            );
        }
        if !(1..=32).contains(&self.radio.bladerf_stream_transfers) {
            bail!("radio.bladerf_stream_transfers must be in 1..=32");
        }
        if !(-10..=5).contains(&self.power_control.downlink.pich_power_offset_db) {
            bail!("power_control.downlink.pich_power_offset_db must be in -10..=5 dB");
        }

        if !(1..=8).contains(&self.receiver.turbo_iterations) {
            bail!("receiver.turbo_iterations must be in 1..=8");
        }

        if let Some(cpu) = self.runtime.dch_cpu
            && cpu >= 1024
        {
            bail!("runtime.dch_cpu must be in 0..=1023");
        }
        if let Some(power) = &self.power_control.downlink.allocation {
            let reserved = [
                power.cpich_percent,
                power.broadcast_percent,
                power.fach_percent,
                power.pch_percent,
                power.pich_percent,
                power.aich_percent,
                power.dedicated_percent,
                power.hsdpa_percent,
                power.hs_scch_percent,
                power.e_agch_percent,
                power.e_hich_percent,
            ]
            .iter()
            .map(|&value| f64::from(value))
            .sum::<f64>();
            if reserved > 100.0 + 0.00001 {
                bail!("power_control.downlink.allocation allocations exceed 100 percent");
            }
        }
        if !self.cell.broadcast_state_file.is_absolute()
            || self.cell.broadcast_state_file.file_name().is_none()
        {
            bail!("cell.broadcast_state_file must be an absolute file path");
        }
        if self.runtime.recovery.backoff_ms > self.runtime.recovery.maximum_backoff_ms {
            bail!("runtime.recovery.backoff_ms must not exceed maximum_backoff_ms");
        }
        if let Some(cpich) = self.cell.advertised_power.cpich_tx_dbm
            && !(-10..=50).contains(&cpich)
        {
            bail!("cell.advertised_power.cpich_tx_dbm must be in -10..=50 dBm");
        }
        if let Some(interference) = self.cell.advertised_power.uplink_interference_dbm
            && !(-110..=-50).contains(&interference)
        {
            bail!("cell.advertised_power.uplink_interference_dbm must be in -110..=-50 dBm");
        }
        if !(self.power_control.downlink.cpich_amplitude > 0.0
            && self.power_control.downlink.cpich_amplitude <= 0.25)
        {
            bail!("power_control.downlink.cpich_amplitude must be greater than 0 and at most 0.25");
        }
        if self.runtime.minimum_submission_us > self.runtime.maximum_submission_us {
            bail!("runtime.minimum_submission_us must not exceed maximum_submission_us");
        }
        if self.runtime.maximum_submission_us > self.runtime.maximum_preparation_us {
            bail!("runtime.maximum_submission_us must not exceed maximum_preparation_us");
        }
        tracing_subscriber::EnvFilter::try_new(&self.diagnostics.logging.filter).map_err(|e| {
            anyhow!(
                "log.filter {:?} is not a valid tracing filter: {e}",
                self.diagnostics.logging.filter
            )
        })?;
        let bind_ip = self
            .core
            .gtpu
            .bind_ip
            .parse::<Ipv4Addr>()
            .with_context(|| {
                format!(
                    "gtpu.bind_ip must be an IPv4 address: {}",
                    self.core.gtpu.bind_ip
                )
            })?;
        let advertise_explicit = self.core.gtpu.advertise_ip.is_some();
        let advertise = self.gtpu_advertise_ip();
        let advertise_ip = advertise
            .parse::<Ipv4Addr>()
            .with_context(|| format!("gtpu.advertise_ip must be an IPv4 address: {advertise}"))?;
        if advertise_ip.is_unspecified() {
            bail!("gtpu.advertise_ip must be a reachable local IPv4 address, not 0.0.0.0");
        }
        if advertise_ip.is_multicast() || advertise_ip.is_broadcast() {
            bail!("gtpu.advertise_ip must be a unicast local IPv4 address");
        }
        if self.core.gtpu.bind_port == 0 {
            bail!("gtpu.bind_port must be in 1..=65535");
        }
        if let Ok(hnbgw_ip) = self.core.hnbgw.address.parse::<Ipv4Addr>() {
            if !hnbgw_ip.is_loopback() && !advertise_explicit {
                bail!(
                    "gtpu.advertise_ip must be explicitly set when hnbgw.address is non-loopback"
                );
            }
            if !hnbgw_ip.is_loopback() && advertise_ip.is_loopback() {
                bail!("gtpu.advertise_ip must not be loopback for a non-loopback HNB-GW");
            }
            if bind_ip.is_loopback() && !hnbgw_ip.is_loopback() {
                bail!("gtpu.bind_ip must not be loopback for a non-loopback HNB-GW");
            }
        }
        if let Some(port) = self.core.hnbgw.udp_encapsulation_port {
            if self.core.hnbgw.transport != HnbgwTransport::Usrsctp {
                bail!("hnbgw.udp_encapsulation_port requires hnbgw.transport = usrsctp");
            }
            if port == 0 {
                bail!("hnbgw.udp_encapsulation_port must be nonzero");
            }
        }
        if self.core.iuh.retry_backoff_secs == 0 {
            bail!("core.iuh.retry_backoff_secs must be positive");
        }
        match self.core.hnbgw.udp_encapsulation_local_port {
            Some(_) if self.core.hnbgw.udp_encapsulation_port.is_none() => {
                bail!("hnbgw.udp_encapsulation_local_port requires hnbgw.udp_encapsulation_port");
            }
            Some(0) => bail!("hnbgw.udp_encapsulation_local_port must be nonzero"),
            _ => {}
        }
        for (name, antenna) in [
            ("radio.tx_antenna", &self.radio.tx_antenna),
            ("radio.rx_antenna", &self.radio.rx_antenna),
        ] {
            if antenna
                .as_deref()
                .is_some_and(|antenna| antenna.trim().is_empty())
            {
                bail!("{name} must not be empty");
            }
        }
        Ok(())
    }

    pub fn gtpu_advertise_ip(&self) -> &str {
        self.core
            .gtpu
            .advertise_ip
            .as_deref()
            .unwrap_or(&self.core.gtpu.bind_ip)
    }
}

impl ManagementConfig {
    pub fn validate_policy(&self) -> Result<()> {
        if !self.socket_path.is_absolute()
            || !self.administrator_socket_path.is_absolute()
            || self.socket_path == self.administrator_socket_path
            || !self.capture_directory.is_absolute()
            || self.capture_directory.parent().is_none()
        {
            bail!(
                "management.grpc requires distinct absolute sockets and an absolute capture directory"
            );
        }
        let certificates = [
            self.tls_certificate.as_ref(),
            self.tls_key.as_ref(),
            self.tls_client_ca.as_ref(),
        ];
        if let Some(address) = &self.tcp_address {
            address
                .parse::<std::net::SocketAddr>()
                .context("management.grpc.tcp_address must be an IP address and port")?;
            if certificates
                .iter()
                .any(|path| path.is_none_or(|path| !path.is_absolute()))
            {
                bail!("management.grpc TCP requires absolute certificate, key and client CA paths");
            }
        } else if certificates.iter().any(|path| path.is_some())
            || !self.administrator_certificates_sha256.is_empty()
        {
            bail!("management.grpc TLS settings require tcp_address");
        }
        if self.administrator_certificates_sha256.len() > 32
            || self
                .administrator_certificates_sha256
                .iter()
                .any(|fingerprint| {
                    fingerprint.len() != 64
                        || !fingerprint
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
        {
            bail!(
                "management.grpc administrator fingerprints must be bounded lowercase SHA256 hex"
            );
        }
        Ok(())
    }
}

/// TS 24.008 10.5.1.3 encodes a two-digit MNC with a 0xf third-digit nibble.
pub fn parse_plmn(s: &str) -> Result<[u8; 3]> {
    let (mcc, mnc) = s
        .split_once('-')
        .ok_or_else(|| anyhow!("plmn '{s}' missing '-' separator (expected MCC-MNC)"))?;

    if mcc.len() != 3 || !mcc.chars().all(|c| c.is_ascii_digit()) {
        bail!("plmn MCC '{mcc}' must be exactly 3 digits");
    }
    if !(mnc.len() == 2 || mnc.len() == 3) || !mnc.chars().all(|c| c.is_ascii_digit()) {
        bail!("plmn MNC '{mnc}' must be 2 or 3 digits");
    }

    let d = |c: char| (c as u8) - b'0';
    let m: Vec<u8> = mcc.chars().map(d).collect();
    let n: Vec<u8> = mnc.chars().map(d).collect();

    let (mnc1, mnc2, mnc3) = match n.len() {
        2 => (n[0], n[1], 0xF),
        3 => (n[0], n[1], n[2]),
        _ => unreachable!(),
    };

    Ok([(m[1] << 4) | m[0], (mnc3 << 4) | m[2], (mnc2 << 4) | mnc1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn dual_cell_requires_explicit_enablement_and_wide_tx_geometry() {
        let mut config = base_cfg();
        assert!(!config.hspa.downlink.dual_cell);
        assert!(!config.cell.secondary_carrier.enabled);
        config.validate_secondary_carrier().unwrap();
        config.hspa.downlink.dual_cell = true;
        assert!(config.validate_secondary_carrier().is_err());
        config.hspa.downlink.enabled = true;
        config.cell.secondary_carrier.enabled = true;
        config.cell.secondary_carrier.uarfcn_dl = Some(config.radio.uarfcn_dl + 25);
        assert!(config.validate_secondary_carrier().is_err());
        config.radio.tx_samples_per_chip = 4;
        config.radio.tx_analog_bandwidth_hz = 10_000_000;
        config.radio.tx_lo_offset_hz = 0;
        config.validate_secondary_carrier().unwrap();
        config.radio.tx_lo_offset_hz = 2_500_000;
        assert!(config.validate_secondary_carrier().is_err());
        config.radio.tx_analog_bandwidth_hz = 15_000_000;
        config.validate_secondary_carrier().unwrap();
        config.radio.tx_lo_offset_hz = i64::MIN;
        assert!(config.validate_secondary_carrier().is_err());
    }

    #[test]
    fn secondary_carrier_power_and_channel_are_checked() {
        let mut config = base_cfg();
        for power in [0.0, 100.0, -1.0, f64::NAN, f64::INFINITY] {
            config.cell.secondary_carrier.power_percent = power;
            assert!(config.validate_secondary_carrier().is_err());
        }
        config.cell.secondary_carrier.power_percent = 50.0;
        config.cell.secondary_carrier.enabled = true;
        config.radio.tx_samples_per_chip = 4;
        config.radio.tx_analog_bandwidth_hz = 10_000_000;
        assert!(config.validate_secondary_carrier().is_err());
        config.cell.secondary_carrier.uarfcn_dl = Some(config.radio.uarfcn_dl + 25);
        config.validate_secondary_carrier().unwrap();
    }

    #[test]
    fn security_policy_defaults_and_empty_lists() {
        let mut config: Config = serde_yaml::from_str(MINIMAL_YAML).unwrap();
        let policy = &config.security;
        assert!(policy.allow_uea0 && policy.allow_uea1 && policy.allow_uea2);
        assert!(policy.allow_uia1 && policy.allow_uia2);
        config.security.allow_uia1 = false;
        config.security.allow_uia2 = false;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("integrity algorithm")
        );
        config.security.allow_uia2 = true;
        config.security.allow_uea0 = false;
        config.security.allow_uea1 = false;
        config.security.allow_uea2 = false;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("ciphering algorithm")
        );
    }

    #[test]
    fn measurement_reporting_defaults_continuous_and_invalid_values() {
        let mut config: Config = serde_yaml::from_str(MINIMAL_YAML).unwrap();
        assert_eq!(
            config.cell.measurements.ue_measurement_report_count,
            Some(32)
        );
        assert_eq!(config.cell.measurements.ue_measurement_interval_ms, 1000);
        let serving: CellMeasurementsSettings = serde_yaml::from_str(
            "ue_measurement_report_count: null\nue_measurement_interval_ms: 250\n",
        )
        .unwrap();
        assert_eq!(serving.ue_measurement_report_count, None);
        assert_eq!(serving.ue_measurement_interval_ms, 250);
        for count in [
            None,
            Some(1),
            Some(2),
            Some(4),
            Some(8),
            Some(16),
            Some(32),
            Some(64),
        ] {
            config.cell.measurements.ue_measurement_report_count = count;
            config.validate().unwrap();
        }
        config.cell.measurements.ue_measurement_report_count = Some(3);
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("ue_measurement_report_count")
        );
        config.cell.measurements.ue_measurement_report_count = None;
        for interval in [0, 1, 750, 65000] {
            config.cell.measurements.ue_measurement_interval_ms = interval;
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("ue_measurement_interval_ms")
            );
        }
    }

    mod recovery;

    #[test]
    fn grpc_management_defaults_are_local_and_have_separate_administrator_access() {
        let config = base_cfg();
        let management = &config.management.grpc;
        assert!(management.enabled);
        assert_eq!(
            management.socket_path,
            std::env::current_dir()
                .unwrap()
                .join(".runtime/management.sock")
        );
        assert_eq!(
            management.administrator_socket_path,
            std::env::current_dir().unwrap().join(".runtime/admin.sock")
        );
        assert!(management.tcp_address.is_none());
        assert!(management.tls_key.is_none());
        assert_eq!(management.maximum_capture_disk_mib, 2048);
        config.validate().unwrap();
    }

    #[test]
    fn grpc_management_rejects_ambiguous_access_and_unsecured_tcp() {
        let mut config = base_cfg();
        config.management.grpc.administrator_socket_path =
            config.management.grpc.socket_path.clone();
        assert!(config.validate().is_err());
        let mut config = base_cfg();
        config.management.grpc.tcp_address = Some("127.0.0.1:50051".into());
        assert!(config.validate().is_err());
        config.management.grpc.tls_certificate = Some(PathBuf::from("/etc/umts-hnb/server.pem"));
        config.management.grpc.tls_key = Some(PathBuf::from("/etc/umts-hnb/server.key"));
        config.management.grpc.tls_client_ca = Some(PathBuf::from("/etc/umts-hnb/client-ca.pem"));
        config.validate().unwrap();
        config
            .management
            .grpc
            .administrator_certificates_sha256
            .push("z".repeat(64));
        assert!(config.validate().is_err());
        config.management.grpc.administrator_certificates_sha256 = vec!["a".repeat(64)];
        config.validate().unwrap();
        config.management.grpc.tcp_address = None;
        assert!(config.validate().is_err());
    }

    #[test]
    fn settings_validate_remaining_ranges() {
        let original = serde_json::to_value(base_cfg()).unwrap();
        for (section, field, invalid) in [
            ("rrc", "srnc_id", 4096),
            ("rrc", "cn_drx_cycle_length_coefficient", 5),
            ("rrc", "max_allowed_ul_tx_power_dbm", 34),
            ("cell", "q_qual_min", -25),
        ] {
            let mut value = original.clone();
            value[section][field] = invalid.into();
            let cfg: Config = serde_json::from_value(value).unwrap();
            assert!(cfg.validate().is_err(), "{section}.{field}");
        }
        for (section, field, invalid) in [
            ("prach", "signature", 16),
            ("prach", "subchannel", 12),
            ("prach", "scrambling_code_word_number", 16),
            ("prach", "dynamic_persistence_level", 0),
            ("common", "aich_power_offset_db", 6),
            ("r99", "initial_ul_puncturing_limit_percent", 0),
            ("r99", "initial_ul_puncturing_limit_percent", 45),
        ] {
            let mut value = original.clone();
            let group = match section {
                "prach" => &mut value["cell"]["access"]["prach"],
                "common" => &mut value["cell"]["common_channels"],
                "r99" => &mut value["rel99"]["physical"],
                _ => unreachable!(),
            };
            group[field] = invalid.into();
            let cfg: Config = serde_json::from_value(value).unwrap();
            assert!(cfg.validate().is_err(), "radio.{section}.{field}");
        }
        let mut cfg = base_cfg();
        cfg.radio.tx_gain_db = f64::NAN;
        assert!(cfg.validate().is_err());
        let mut cfg = base_cfg();
        cfg.cell.common_channels.aich_power_offset_db = -22;
        cfg.cell.q_qual_min = -24;
        cfg.rrc.cn_drx_cycle_length_coefficient = 9;
        cfg.cell.access.prach.dynamic_persistence_level = 8;
        cfg.validate().unwrap();
    }

    #[test]
    fn serving_si_state_is_an_absolute_file_without_preflight_io() {
        let mut cfg = base_cfg();
        for path in ["", "relative.bin", "/"] {
            cfg.cell.broadcast_state_file = path.into();
            assert!(cfg.validate().is_err());
        }
        cfg.cell.broadcast_state_file = "/nonexistent-umts-si-state-parent/revision.pb".into();
        cfg.validate().unwrap();
        assert!(!cfg.cell.broadcast_state_file.exists());
    }

    #[test]
    fn retired_unused_paging_timer_is_rejected() {
        for value in [5000, 1000] {
            let mut document = serde_json::to_value(base_cfg()).unwrap();
            document["rrc"]["default_paging_timeout_ms"] = serde_json::json!(value);
            let error = serde_yaml::from_str::<Config>(&serde_json::to_string(&document).unwrap())
                .unwrap_err();
            assert!(error.to_string().contains("unknown field"), "{error}");
            assert!(
                error.to_string().contains("default_paging_timeout_ms"),
                "{error}"
            );
        }
    }

    #[test]
    fn config_rejects_unknown_and_duplicate_fields_at_every_struct_level() {
        let cfg = serde_json::to_value(base_cfg()).unwrap();
        let mut object_paths = Vec::new();
        fn collect(value: &serde_json::Value, path: &str, paths: &mut Vec<String>) {
            if let Some(object) = value.as_object() {
                paths.push(path.to_owned());
                for (key, value) in object {
                    collect(value, &format!("{path}/{key}"), paths);
                }
            } else if let Some(array) = value.as_array() {
                for (index, value) in array.iter().enumerate() {
                    collect(value, &format!("{path}/{index}"), paths);
                }
            }
        }
        collect(&cfg, "", &mut object_paths);
        for path in object_paths {
            let mut invalid = cfg.clone();
            invalid
                .pointer_mut(&path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unknown_setting".into(), serde_json::json!(true));
            let error = serde_yaml::from_str::<Config>(&serde_yaml::to_string(&invalid).unwrap())
                .unwrap_err();
            assert!(
                error.to_string().contains("unknown field"),
                "{path}: {error}"
            );

            let object = cfg.pointer(&path).unwrap().as_object().unwrap();
            let (key, value) = object.iter().next().unwrap();
            let serialized = serde_json::to_string(object).unwrap();
            let duplicate = format!(
                "{},{}:{}}}",
                serialized.strip_suffix('}').unwrap(),
                serde_json::to_string(key).unwrap(),
                value
            );
            let mut marked = cfg.clone();
            *marked.pointer_mut(&path).unwrap() = "__duplicate_object_here__".into();
            let yaml = serde_json::to_string(&marked)
                .unwrap()
                .replace("\"__duplicate_object_here__\"", &duplicate);
            let error = serde_yaml::from_str::<Config>(&yaml).unwrap_err();
            assert!(
                error.to_string().contains("duplicate field"),
                "{path}: {error}"
            );
        }
        let yaml = serde_yaml::to_string(&base_cfg()).unwrap();
        let duplicate = format!("{yaml}\nbest_effort_bytes_per_sec: 60000\n");
        assert!(serde_yaml::from_str::<Config>(&duplicate).is_err());
    }

    #[test]
    fn plmn_test_network() {
        assert_eq!(parse_plmn("001-01").unwrap(), [0x00, 0xF1, 0x10]);
    }

    #[test]
    fn plmn_three_digit_mnc() {
        assert_eq!(parse_plmn("262-070").unwrap(), [0x62, 0x02, 0x70]);
    }

    #[test]
    fn plmn_rejects_garbage() {
        assert!(parse_plmn("00101").is_err());
        assert!(parse_plmn("abc-01").is_err());
        assert!(parse_plmn("001-1").is_err());
        assert!(parse_plmn("001-1234").is_err());
    }

    fn base_cfg() -> Config {
        let mut config: Config = serde_yaml::from_str(MINIMAL_YAML).unwrap();
        config.cell.q_qual_min = -24;
        config.cell.plmn = "001-01".into();
        config.cell.lac = 1;
        config.cell.rac = 1;
        config.cell.sac = 1;
        config.cell.cell_id = 1;
        config.cell.hnb_identity = "hnb".into();
        config.cell.gprs_enable = true;
        config.cell.nmo = NetworkOperationMode::SeparatePaging;
        config.core.hnbgw.address = "127.0.0.1".into();
        config.core.hnbgw.port = 29169;
        config.core.hnbgw.transport = HnbgwTransport::Sctp;
        config.core.gtpu.bind_ip = "127.0.0.1".into();
        config.core.gtpu.bind_port = 2152;
        config.core.gtpu.advertise_ip = None;
        config.core.gtpu.allow_ephemeral_port = false;
        config.core.gtpu.release_rab_on_error_indication = false;
        config.radio.uhd_rx_timestamp_offset = None;
        config.radio.backend = "soapy".into();
        config.radio.soapy_args = "driver=uhd".into();
        config.radio.uhd_args = String::new();
        config.radio.bladerf_args = String::new();
        config.radio.bladerf_rx_sample_phase = 0;
        config.radio.uarfcn_dl = 10637;
        config.radio.uarfcn_ul = 9687;
        config.radio.tx_gain_db = 60.0;
        config.radio.rx_gain_db = 40.0;
        config.cell.physical.downlink_scrambling_code = 469;
        config.cell.physical.dch_secondary_scrambling_code = 0;
        config.hspa.downlink.enabled = false;
        config.hspa.downlink.fdpch = false;
        config.hspa.downlink.compact_common_channels = false;
        config.hspa.downlink.measurement_power_offset_db = Some(6.0);
        config.hspa.downlink.feedback.ack_nack_power_index = 3;
        config.hspa.downlink.feedback.cqi_power_index = 8;
        config.hspa.downlink.feedback.cqi_interval_ms = 2;
        config.power_control.uplink.sir_ceiling_db = 34.7712;
        config.power_control.uplink.initial_sir_db = None;
        config.power_control.uplink.sir_floor_db = None;
        config.power_control.uplink.target_bler = 0.02;
        config.power_control.uplink.tpc_hold_groups = 2;
        config.hspa.downlink.harq_processes = 6;
        config.hspa.downlink.harq_memory_bits = Vec::new();
        config.hspa.downlink.secondary_scrambling_code = 0;
        config.hspa.downlink.all_secondary_power_offset_db = 0.0;
        config.hspa.uplink.enabled = false;
        config.hspa.uplink.scheduled = true;
        config.hspa.uplink.four_code_uplink = false;
        config.hspa.uplink.rv_zero_only = false;
        config.hspa.uplink.max_category = 1;
        config.hspa.uplink.initial_serving_grant = None;
        config.hspa.uplink.serving_grant = 31;
        config.hspa.uplink.edpcch_power_offset = 5;
        config.hspa.uplink.edpdch_references = default_hsupa_edpdch_references();
        config.radio.bladerf_stream_buffers = 8;
        config.radio.bladerf_stream_buffer_samples = 4096;
        config.radio.bladerf_stream_transfers = 4;
        config.radio.bladerf_rx_timestamp_offset = None;
        config.power_control.downlink.pich_power_offset_db = -10;
        config.receiver.turbo_iterations = 4;
        config
    }

    #[test]
    fn validate_rejects_oversized_cell_id() {
        let mut c = base_cfg();
        c.cell.cell_id = 1 << 28;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_hsdpa_rlc_geometry() {
        let c = base_cfg();
        assert_eq!(c.hspa.downlink.rlc_pdu_bits, 4640);
        assert_eq!(c.hspa.downlink.rlc_receive_window, 768);
        c.validate().unwrap();
        for bits in [4990, 16, 5008] {
            let mut c = base_cfg();
            c.hspa.downlink.rlc_pdu_bits = bits;
            assert!(c.validate().is_err(), "{bits}");
        }
        for window in [0, 2048] {
            let mut c = base_cfg();
            c.hspa.downlink.rlc_receive_window = window;
            assert!(c.validate().is_err(), "{window}");
        }
    }

    #[test]
    fn validate_explicit_harq_memory_partition() {
        let mut c = base_cfg();
        c.hspa.downlink.harq_processes = 8;
        c.hspa.downlink.harq_memory_bits = vec![20_800; 8];
        assert!(c.validate().is_ok());
        c.hspa.downlink.harq_memory_bits.pop();
        assert!(c.validate().is_err());
        c.hspa.downlink.harq_memory_bits = vec![20_800; 8];
        c.hspa.downlink.harq_memory_bits[3] = 21_600;
        assert!(c.validate().is_err());
    }

    #[test]
    fn scheduler_cell_budget_accepts_multi_gib_and_preserves_ue_ceiling() {
        let defaults: serde_yaml::Value = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../config/defaults.yaml"
        )))
        .unwrap();
        let mut scheduling: RustHsdpaScheduling =
            serde_yaml::from_value(defaults["hspa"]["downlink"]["resources"]["scheduling"].clone())
                .unwrap();
        for limit in [640_000_000, 1_073_741_824] {
            scheduling.maximum_ue_bytes = limit;
            scheduling.maximum_cell_bytes = 1_073_741_824;
            scheduling.validate_schema().unwrap();
        }
        scheduling.maximum_ue_bytes = 1_073_741_825;
        assert!(scheduling.validate_schema().is_err());
        scheduling.maximum_ue_bytes = 640_000_000;
        for limit in [
            1_073_741_825,
            2_147_483_648,
            2_684_354_560,
            3_221_225_472,
            u32::MAX,
        ] {
            scheduling.maximum_cell_bytes = limit;
            scheduling.validate_schema().unwrap();
        }
        scheduling.maximum_cell_bytes = 0;
        assert!(scheduling.validate_schema().is_err());
    }

    #[test]
    fn validate_rejects_mixed_secondary_hs_and_dpch_topology() {
        let mut c = base_cfg();
        c.hspa.downlink.enabled = true;
        c.cell.physical.dch_secondary_scrambling_code = 1;
        c.hspa.downlink.secondary_scrambling_code = 2;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_accepts_equal_all_secondary_fdpch_hspa_topology() {
        let mut c = base_cfg();
        c.hspa.downlink.enabled = true;
        c.hspa.downlink.fdpch = true;
        c.hspa.uplink.enabled = true;
        c.cell.physical.dch_secondary_scrambling_code = 1;
        c.hspa.downlink.secondary_scrambling_code = 1;
        c.hspa.downlink.all_secondary_power_offset_db = 3.0;
        assert!(c.validate().is_ok());
        c.hspa.downlink.fdpch = false;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_compact_common_channels_allows_primary_dpch_or_fdpch() {
        let mut c = base_cfg();
        assert!(!c.hspa.downlink.compact_common_channels);
        c.hspa.downlink.compact_common_channels = true;
        assert!(c.validate().is_err());
        c.hspa.downlink.enabled = true;
        assert!(c.validate().is_ok());
        c.hspa.uplink.enabled = true;
        assert!(c.validate().is_ok());
        c.hspa.downlink.fdpch = true;
        assert!(c.validate().is_ok());
        c.hspa.uplink.enabled = true;
        assert!(c.validate().is_ok());
        c.cell.physical.dch_secondary_scrambling_code = 1;
        assert!(c.validate().is_err());
        c.hspa.downlink.secondary_scrambling_code = 1;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_fdpch_requires_independent_primary_hs_construction() {
        let mut c = base_cfg();
        c.hspa.downlink.enabled = true;
        c.hspa.downlink.fdpch = true;
        assert!(c.validate().is_ok());
        c.hspa.uplink.enabled = true;
        assert!(c.validate().is_ok());
        c.cell.physical.dch_secondary_scrambling_code = 1;
        assert!(c.validate().is_ok());
        c.hspa.downlink.secondary_scrambling_code = 1;
        assert!(c.validate().is_ok());
        c.hspa.uplink.enabled = false;
        assert!(c.validate().is_err());
        c.hspa.downlink.secondary_scrambling_code = 0;
        c.cell.physical.dch_secondary_scrambling_code = 0;
        c.hspa.downlink.secondary_scrambling_code = 1;
        assert!(c.validate().is_err());
        c.hspa.downlink.secondary_scrambling_code = 0;
        c.cell.physical.dch_secondary_scrambling_code = 1;
        assert!(c.validate().is_err());
        c.cell.physical.dch_secondary_scrambling_code = 0;
        c.hspa.downlink.enabled = false;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_primary_scrambling_code_above_fdd_range() {
        let mut c = base_cfg();
        c.cell.physical.downlink_scrambling_code = 512;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_bounds_bladerf_rx_sample_phase() {
        let mut c = base_cfg();
        c.radio.bladerf_rx_sample_phase = 1;
        assert!(c.validate().is_ok());
        c.radio.bladerf_rx_sample_phase = 2;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_lac() {
        let mut c = base_cfg();
        c.cell.lac = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_rac() {
        let mut c = base_cfg();
        c.cell.rac = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_gtpu_bind_port() {
        let mut c = base_cfg();
        c.core.gtpu.bind_port = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_bounds_hsupa_serving_grant() {
        let mut c = base_cfg();
        c.hspa.uplink.serving_grant = 2;
        assert!(c.validate().is_ok());
        c.hspa.uplink.initial_serving_grant = Some(2);
        assert!(c.validate().is_ok());
        c.hspa.uplink.serving_grant = 31;
        c.hspa.uplink.initial_serving_grant = Some(21);
        assert!(c.validate().is_ok());
        c.hspa.uplink.initial_serving_grant = Some(1);
        assert!(c.validate().is_err());
        c.hspa.uplink.initial_serving_grant = Some(30);
        c.hspa.uplink.serving_grant = 29;
        assert!(c.validate().is_err());
        c.hspa.uplink.initial_serving_grant = None;
        c.hspa.uplink.serving_grant = 1;
        assert!(c.validate().is_err());
        c.hspa.uplink.serving_grant = 32;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_edpdch_reference_points() {
        let mut c = base_cfg();
        c.hspa.uplink.edpdch_references = vec![
            EdpdchReference {
                etfci: 11,
                power_offset: 4,
            },
            EdpdchReference {
                etfci: 83,
                power_offset: 29,
            },
        ];
        assert!(c.validate().is_ok());
        c.hspa.uplink.edpdch_references[1].power_offset = 30;
        assert!(c.validate().is_err());
        c.hspa.uplink.edpdch_references[1] = EdpdchReference {
            etfci: 11,
            power_offset: 16,
        };
        assert!(c.validate().is_err());
        c.hspa.uplink.edpdch_references.clear();
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_bounds_hsdpa_measurement_power_offset() {
        let mut c = base_cfg();
        c.hspa.downlink.measurement_power_offset_db = None;
        assert!(c.validate().is_ok());
        for gamma in [-6.0, 0.0, 6.0, 13.0] {
            c.hspa.downlink.measurement_power_offset_db = Some(gamma);
            assert!(c.validate().is_ok());
        }
        for gamma in [-6.5, 13.5, 6.25, f64::NAN] {
            c.hspa.downlink.measurement_power_offset_db = Some(gamma);
            assert!(c.validate().is_err());
        }
    }

    #[test]
    fn validate_bounds_hs_dpcch_feedback_delta() {
        let mut c = base_cfg();
        c.hspa.downlink.feedback.ack_nack_power_index = 0;
        assert!(c.validate().is_ok());
        c.hspa.downlink.feedback.ack_nack_power_index = 8;
        assert!(c.validate().is_ok());
        c.hspa.downlink.feedback.ack_nack_power_index = 9;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_cqi_feedback_power_and_cycle() {
        let mut c = base_cfg();
        for delta in 0..=8 {
            c.hspa.downlink.feedback.cqi_power_index = delta;
            assert!(c.validate().is_ok());
        }
        c.hspa.downlink.feedback.cqi_power_index = 9;
        assert!(c.validate().is_err());
        c.hspa.downlink.feedback.cqi_power_index = 8;
        for cycle in [2, 4, 8, 10, 20, 40, 80, 160] {
            c.hspa.downlink.feedback.cqi_interval_ms = cycle;
            assert!(c.validate().is_ok());
        }
        for cycle in [0, 1, 3, 6, 161] {
            c.hspa.downlink.feedback.cqi_interval_ms = cycle;
            assert!(c.validate().is_err());
        }
    }

    #[test]
    fn primary_grant_refresh_is_optional_and_bounded() {
        let mut config: Config = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-support/fixtures/config/hspa-two-ue.yaml"
        )))
        .unwrap();
        assert_eq!(
            config
                .hspa
                .uplink
                .resources
                .as_ref()
                .unwrap()
                .primary_grant_refresh_ms,
            None
        );
        for period in [20, 1000, 120000] {
            config
                .hspa
                .uplink
                .resources
                .as_mut()
                .unwrap()
                .primary_grant_refresh_ms = Some(period);
            config.validate().unwrap();
        }
        for period in [0, 19, 120001] {
            config
                .hspa
                .uplink
                .resources
                .as_mut()
                .unwrap()
                .primary_grant_refresh_ms = Some(period);
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn validate_bounds_uplink_dpcch_sir_target() {
        let mut c = base_cfg();
        for target in [0.0, 6.0, 10.0, 17.0, 31.7609, 45.0] {
            c.power_control.uplink.sir_ceiling_db = target;
            assert!(c.validate().is_ok());
        }
        for target in [-0.1, 45.1, f64::NAN] {
            c.power_control.uplink.sir_ceiling_db = target;
            assert!(c.validate().is_err());
        }
    }

    #[test]
    fn validate_bounds_uplink_dpcch_outer_loop() {
        let mut c = base_cfg();
        c.power_control.uplink.sir_ceiling_db = 21.0;
        for floor in [
            Some(0.0),
            Some(3.0),
            Some(10.0),
            Some(17.0),
            Some(21.0),
            None,
        ] {
            c.power_control.uplink.sir_floor_db = floor;
            assert!(c.validate().is_ok());
        }
        for floor in [Some(-0.1), Some(21.1), Some(f64::NAN)] {
            c.power_control.uplink.sir_floor_db = floor;
            assert!(c.validate().is_err());
        }
        c.power_control.uplink.sir_floor_db = Some(17.0);
        for target_bler in [0.01, 0.10, 0.50] {
            c.power_control.uplink.target_bler = target_bler;
            assert!(c.validate().is_ok());
        }
        for target_bler in [0.0, 0.51, f64::NAN] {
            c.power_control.uplink.target_bler = target_bler;
            assert!(c.validate().is_err());
        }
    }

    #[test]
    fn initial_uplink_sir_is_independent_but_bounded() {
        let mut c = base_cfg();
        c.power_control.uplink.sir_ceiling_db = 21.0;
        c.power_control.uplink.sir_floor_db = Some(12.0);
        for initial in [None, Some(12.0), Some(17.0), Some(21.0)] {
            c.power_control.uplink.initial_sir_db = initial;
            assert!(c.validate().is_ok());
        }
        for initial in [11.9, 21.1, f64::NAN, f64::INFINITY] {
            c.power_control.uplink.initial_sir_db = Some(initial);
            assert!(c.validate().is_err());
        }
        c.power_control.uplink.sir_floor_db = None;
        c.power_control.uplink.initial_sir_db = Some(12.0);
        assert!(c.validate().is_err()); // omitted floor means fixed target
        c.power_control.uplink.initial_sir_db = Some(21.0);
        assert!(c.validate().is_ok());
        let seed_yaml = |initial, floor| {
            let mut c = base_cfg();
            c.power_control.uplink.sir_ceiling_db = 14.0;
            c.power_control.uplink.sir_floor_db = Some(floor);
            c.power_control.uplink.initial_sir_db = Some(initial);
            serde_yaml::to_string(&c).unwrap()
        };
        for initial in [0.0, 6.0, 7.0, 10.0, 14.0] {
            let yaml = seed_yaml(initial, 0.0);
            let parsed: Config = serde_yaml::from_str(&yaml).unwrap();
            assert!(parsed.validate().is_ok(), "seed {initial}");
        }
        for initial in [-0.1, 14.1, 46.0] {
            let yaml = seed_yaml(initial, 0.0);
            let result = serde_yaml::from_str::<Config>(&yaml);
            assert!(result.map(|c| c.validate().is_err()).unwrap_or(true));
        }
        assert!(
            serde_yaml::from_str::<Config>(&seed_yaml(5.0, 6.0))
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn validate_bounds_uplink_dpcch_tpc_hold_groups() {
        let mut c = base_cfg();
        for hold_groups in [0, 2, 8] {
            c.power_control.uplink.tpc_hold_groups = hold_groups;
            assert!(c.validate().is_ok());
        }
        c.power_control.uplink.tpc_hold_groups = 9;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_unspecified_gtpu_advertise_ip() {
        let mut c = base_cfg();
        c.core.gtpu.bind_ip = "0.0.0.0".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_allows_unspecified_gtpu_bind_with_advertise_ip() {
        let mut c = base_cfg();
        c.core.gtpu.bind_ip = "0.0.0.0".into();
        c.core.gtpu.advertise_ip = Some("127.0.0.1".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validate_rejects_non_loopback_hnbgw_without_explicit_gtpu_advertise() {
        let mut c = base_cfg();
        c.core.hnbgw.address = "192.0.2.10".into();
        c.core.gtpu.bind_ip = "192.0.2.20".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_allows_non_loopback_hnbgw_with_explicit_gtpu_advertise() {
        let mut c = base_cfg();
        c.core.hnbgw.address = "192.0.2.10".into();
        c.core.gtpu.bind_ip = "192.0.2.20".into();
        c.core.gtpu.advertise_ip = Some("192.0.2.20".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validate_udp_encapsulation_requires_usrsctp_and_nonzero_port() {
        let mut c = base_cfg();
        c.core.hnbgw.udp_encapsulation_port = Some(9899);
        assert!(c.validate().is_err());
        c.core.hnbgw.transport = HnbgwTransport::Usrsctp;
        assert!(c.validate().is_ok());
        c.core.hnbgw.udp_encapsulation_port = Some(0);
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_antenna_names() {
        let mut c = base_cfg();
        c.radio.tx_antenna = Some("TX/RX".into());
        c.radio.rx_antenna = Some("RX2".into());
        assert!(c.validate().is_ok());
        c.radio.rx_antenna = Some(" ".into());
        assert!(c.validate().is_err());
        c.radio.rx_antenna = None;
        c.radio.tx_antenna = Some(String::new());
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_core_retry_backoff() {
        let mut c = base_cfg();
        c.core.iuh.retry_backoff_secs = 0;
        assert!(c.validate().is_err());
        c.core.iuh.retry_backoff_secs = 1;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validate_udp_encapsulation_local_port_requires_remote_and_nonzero_port() {
        let mut c = base_cfg();
        c.core.hnbgw.transport = HnbgwTransport::Usrsctp;
        c.core.hnbgw.udp_encapsulation_local_port = Some(9900);
        assert!(c.validate().is_err());
        c.core.hnbgw.udp_encapsulation_port = Some(9899);
        assert!(c.validate().is_ok());
        c.core.hnbgw.udp_encapsulation_local_port = Some(0);
        assert!(c.validate().is_err());
    }

    const MINIMAL_YAML: &str = r#"
cell:
  plmn: "001-01"
  lac: 1
  rac: 1
  sac: 1
  cell_id: 1
  hnb_identity: "hnb"
core:
  hnbgw:
    address: "127.0.0.1"
  gtpu:
    bind_ip: "127.0.0.1"
radio:
  uarfcn_dl: 10637
  uarfcn_ul: 9687
  tx_gain_db: 60.0
  rx_gain_db: 40.0
"#;

    #[test]
    fn removed_native_tx_scale_is_rejected() {
        let yaml = format!("{MINIMAL_YAML}  tx_composite_scale: 25\n");
        let error = serde_yaml::from_str::<Config>(&yaml).unwrap_err();
        assert!(error.to_string().contains("tx_composite_scale"));
        let config: Config = serde_yaml::from_str(MINIMAL_YAML).unwrap();
        assert!(
            serde_json::to_value(config).unwrap()["radio"]
                .get("tx_composite_scale")
                .is_none()
        );
    }

    #[test]
    fn runtime_knob_defaults_match_former_env_defaults() {
        let c: Config = serde_yaml::from_str(MINIMAL_YAML).expect("minimal YAML parses");
        c.validate().expect("defaults validate");
        assert_eq!(c.radio.bladerf_stream_buffers, 8);
        assert_eq!(c.radio.bladerf_stream_buffer_samples, 4096);
        assert_eq!(c.radio.bladerf_stream_transfers, 4);
        assert_eq!(c.radio.bladerf_rx_timestamp_offset, None);
        assert_eq!(c.power_control.downlink.pich_power_offset_db, -10);
        assert_eq!(c.receiver.turbo_iterations, 4);
        assert_eq!(c.runtime.dch_cpu, None);
        assert_eq!(c.runtime.simd, SimdMode::Auto);
        assert!(!c.core.gtpu.allow_ephemeral_port);
        assert!(c.core.gtpu.release_rab_on_error_indication);
        assert_eq!(c.diagnostics.logging.filter, "info");
    }

    #[test]
    fn runtime_knobs_parse_from_yaml_sections() {
        let yaml = format!(
            "{MINIMAL_YAML}  bladerf_rx_timestamp_offset: -12\nreceiver:\n  turbo_iterations: 2\nruntime:\n  dch_cpu: 3\n  simd: scalar\ndiagnostics:\n  logging:\n    filter: \"umts_hnb=debug,info\"\n"
        );
        let c: Config = serde_yaml::from_str(&yaml).expect("YAML parses");
        c.validate().expect("validates");
        assert_eq!(c.receiver.turbo_iterations, 2);
        assert_eq!(c.radio.bladerf_rx_timestamp_offset, Some(-12));
        assert_eq!(c.runtime.dch_cpu, Some(3));
        assert_eq!(c.runtime.simd, SimdMode::Scalar);
        assert_eq!(c.diagnostics.logging.filter, "umts_hnb=debug,info");
    }

    #[test]
    fn validate_bounds_bladerf_stream_parameters() {
        let mut c = base_cfg();
        for buffers in [4, 8, 64] {
            c.radio.bladerf_stream_buffers = buffers;
            assert!(c.validate().is_ok());
        }
        for buffers in [3, 65] {
            c.radio.bladerf_stream_buffers = buffers;
            assert!(c.validate().is_err());
        }
        c = base_cfg();
        for samples in [1024, 4096, 32768] {
            c.radio.bladerf_stream_buffer_samples = samples;
            assert!(c.validate().is_ok());
        }
        for samples in [0, 512, 4097, 33792] {
            c.radio.bladerf_stream_buffer_samples = samples;
            assert!(c.validate().is_err());
        }
        c = base_cfg();
        for transfers in [1, 4, 32] {
            c.radio.bladerf_stream_transfers = transfers;
            assert!(c.validate().is_ok());
        }
        for transfers in [0, 33] {
            c.radio.bladerf_stream_transfers = transfers;
            assert!(c.validate().is_err());
        }
    }

    #[test]
    fn validate_bounds_pich_power_offset() {
        let mut c = base_cfg();
        for offset in [-10, -5, 0, 5] {
            c.power_control.downlink.pich_power_offset_db = offset;
            assert!(c.validate().is_ok());
        }
        for offset in [-11, 6] {
            c.power_control.downlink.pich_power_offset_db = offset;
            assert!(c.validate().is_err());
        }
    }

    #[test]
    fn validate_bounds_turbo_iterations() {
        let mut c = base_cfg();
        for iterations in [1, 4, 8] {
            c.receiver.turbo_iterations = iterations;
            assert!(c.validate().is_ok());
        }
        for iterations in [0, 9] {
            c.receiver.turbo_iterations = iterations;
            assert!(c.validate().is_err());
        }
    }

    #[test]
    fn validate_bounds_runtime_dch_cpu_and_simd() {
        let mut c = base_cfg();
        for cpu in [None, Some(0), Some(1023)] {
            c.runtime.dch_cpu = cpu;
            assert!(c.validate().is_ok());
        }
        c.runtime.dch_cpu = Some(1024);
        assert!(c.validate().is_err());
        c = base_cfg();
        for simd in [SimdMode::Auto, SimdMode::Scalar] {
            c.runtime.simd = simd;
            assert!(c.validate().is_ok());
        }
        for simd in ["avx2", "AUTO", "", "none"] {
            let mut invalid = serde_json::to_value(&c).unwrap();
            invalid["runtime"]["simd"] = simd.into();
            assert!(serde_json::from_value::<Config>(invalid).is_err());
        }
    }

    #[test]
    fn validate_rejects_malformed_log_filter() {
        let mut c = base_cfg();
        c.diagnostics.logging.filter = "umts_hnb=debug,info".into();
        assert!(c.validate().is_ok());
        c.diagnostics.logging.filter = "umts_hnb=notalevel".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn public_configuration_fixtures_roundtrip_without_setting_loss() {
        let directory =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-support/fixtures/config");
        let mut checked = 0;
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|value| value.to_str()) != Some("yaml") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let original: Config =
                if path.file_name().and_then(|name| name.to_str()) == Some("operator.yaml") {
                    deployment::Deployment::parse(
                        include_str!("../../../../config/defaults.yaml"),
                        &source,
                    )
                    .unwrap()
                    .config
                } else {
                    let config: Config = serde_yaml::from_str(&source).unwrap();
                    if config.cell.packet_modes.is_some() {
                        Config::load(&path).unwrap()
                    } else {
                        config
                    }
                };
            original
                .validate()
                .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
            let restored: Config =
                serde_yaml::from_str(&serde_yaml::to_string(&original).unwrap()).unwrap();
            assert_eq!(
                serde_json::to_value(original).unwrap(),
                serde_json::to_value(restored).unwrap(),
                "{}",
                path.display()
            );
            checked += 1;
        }
        assert!(checked >= 10);
    }
}
