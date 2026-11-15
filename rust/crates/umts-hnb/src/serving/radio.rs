// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail, ensure};
use sdr_hal::config::{DeviceConfig, RadioConfig, RatePolicy};
use umts_dsp::pulse_shaping::SamplesPerChip;

const SAMPLE_RATE_HZ: usize = 7_680_000;
const ANALOG_BANDWIDTH_HZ: usize = 5_000_000;
const UHD_TX_ANTENNA: &str = "TX/RX";
const UHD_RX_ANTENNA: &str = "RX2";
pub(super) struct RadioPlan {
    pub hal: RadioConfig,
    pub tx_samples_per_chip: SamplesPerChip,
    pub downlink_hz: u64,
    pub uplink_hz: u64,
    pub rx_phase: usize,
}

pub(super) fn tx_samples_per_chip(config: &umts_config::Config) -> Result<SamplesPerChip> {
    match config.radio.tx_samples_per_chip {
        2 => Ok(SamplesPerChip::Two),
        4 => Ok(SamplesPerChip::Four),
        _ => bail!("radio.tx_samples_per_chip must be 2 or 4"),
    }
}

fn paired_frequencies(downlink: u32, uplink: u32) -> Result<(u64, u64)> {
    let assignment = crate::radio_assignment::paired_assignment(downlink, uplink)?;
    Ok((assignment.downlink_hz, assignment.uplink_hz))
}

fn argument<'a>(args: &'a str, key: &str) -> Result<Option<&'a str>> {
    let mut selected = None;
    for field in args.split(',') {
        if let Some((name, value)) = field.split_once('=')
            && name.trim() == key
        {
            ensure!(selected.is_none(), "duplicate radio argument {key}");
            ensure!(!value.trim().is_empty(), "empty radio argument {key}");
            selected = Some(value.trim());
        }
    }
    Ok(selected)
}

fn integer_gain(gain: f64) -> Result<i32> {
    ensure!(
        gain.is_finite()
            && gain.fract() == 0.0
            && gain >= f64::from(i32::MIN)
            && gain <= f64::from(i32::MAX),
        "bladeRF gain must be an exactly representable integer dB value"
    );
    Ok(gain as i32)
}

pub(super) fn radio_plan(config: &umts_config::Config) -> Result<RadioPlan> {
    let radio = &config.radio;
    let tx_samples_per_chip = tx_samples_per_chip(config)?;
    let signal_bandwidth = if config.cell.secondary_carrier.enabled {
        2 * ANALOG_BANDWIDTH_HZ
    } else {
        ANALOG_BANDWIDTH_HZ
    };
    let occupied = radio
        .tx_lo_offset_hz
        .unsigned_abs()
        .checked_mul(2)
        .and_then(|offset| offset.checked_add(signal_bandwidth as u64))
        .context("TX LO offset bandwidth overflow")?;
    ensure!(
        occupied <= u64::from(radio.tx_analog_bandwidth_hz),
        "radio.tx_analog_bandwidth_hz must encompass all configured carriers and twice the TX LO offset"
    );
    ensure!(
        radio.tx_gain_db.is_finite() && radio.rx_gain_db.is_finite(),
        "radio gains must be finite"
    );
    let (downlink_hz, uplink_hz) = if config.cell.secondary_carrier.enabled {
        ensure!(
            radio.tx_samples_per_chip == 4,
            "secondary carrier requires four TX samples per chip"
        );
        let assignment = crate::radio_assignment::dual_downlink_assignment(
            radio.uarfcn_dl,
            radio.uarfcn_ul,
            config
                .cell
                .secondary_carrier
                .uarfcn_dl
                .context("secondary downlink UARFCN missing")?,
        )?;
        (assignment.center_hz, assignment.primary.uplink_hz)
    } else {
        paired_frequencies(radio.uarfcn_dl, radio.uarfcn_ul)?
    };
    let (device, rx_phase) = match radio.backend.as_str() {
        "uhd" => {
            ensure!(
                radio.uhd_rx_timestamp_offset.unwrap_or(0) == 0,
                "native UHD RX sample timestamp offsets are not supported by the HAL serving runtime"
            );
            let clock = argument(&radio.uhd_args, "master_clock_rate")?
                .map(|clock| {
                    let clock = clock
                        .parse::<f64>()
                        .context("invalid UHD master_clock_rate")?;
                    ensure!(
                        clock.is_finite()
                            && clock > 0.0
                            && clock.fract() == 0.0
                            && clock <= f64::from(u32::MAX),
                        "UHD master_clock_rate must be a positive integral Hz value within u32"
                    );
                    ensure!(
                        f64::from(radio.tx_analog_bandwidth_hz) <= clock,
                        "radio.tx_analog_bandwidth_hz must not exceed the UHD master clock"
                    );
                    Ok(clock as u64)
                })
                .transpose()?;
            (
                DeviceConfig::Uhd {
                    device: radio.uhd_args.clone(),
                    software_tx_frequency_correction: radio.tx_lo_offset_hz == 0,
                    channel: 0,
                    antenna: radio
                        .tx_antenna
                        .clone()
                        .unwrap_or_else(|| UHD_TX_ANTENNA.to_owned()),
                    tx_gain_db: radio.tx_gain_db,
                    tx_max_gain_db: None,
                    tx_max_gain_power_estimate_dbm: None,
                    master_clock_rate: clock,
                    max_master_clock_rate: None,
                    clock_source: None,
                    time_source: None,
                    rx_antenna: Some(
                        radio
                            .rx_antenna
                            .clone()
                            .unwrap_or_else(|| UHD_RX_ANTENNA.to_owned()),
                    ),
                    rx_gain_db: Some(radio.rx_gain_db),
                    rx_reference_dbm: None,
                    rx_max_gain_db: None,
                    rx_max_gain_reference_dbm: None,
                    rx_to_tx_delay_ns: None,
                },
                0,
            )
        }
        "soapy" => (
            DeviceConfig::Soapy {
                device: radio.soapy_args.clone(),
                channel: 0,
                antenna: radio
                    .tx_antenna
                    .clone()
                    .context("Soapy serving requires radio.tx_antenna")?,
                tx_gain_db: radio.tx_gain_db,
                rx_antenna: Some(
                    radio
                        .rx_antenna
                        .clone()
                        .context("Soapy serving requires radio.rx_antenna")?,
                ),
                rx_gain_db: Some(radio.rx_gain_db),
                rx_reference_dbm: None,
                rx_to_tx_delay_ns: None,
            },
            0,
        ),
        "bladerf" => {
            ensure!(
                radio.bladerf_rx_timestamp_offset.unwrap_or(0) == 0,
                "native bladeRF RX sample timestamp offsets are not supported by the HAL serving runtime"
            );
            ensure!(
                radio.bladerf_rx_sample_phase < 2,
                "bladeRF RX phase must be zero or one at 2x chip rate"
            );
            (
                DeviceConfig::BladeRf {
                    device: radio.bladerf_args.clone(),
                    channel: 0,
                    fpga_path: None,
                    tx_antenna: radio.tx_antenna.clone(),
                    rx_antenna: radio.rx_antenna.clone(),
                    tx_gain_db: integer_gain(radio.tx_gain_db)?,
                    rx_gain_db: Some(integer_gain(radio.rx_gain_db)?),
                    rx_reference_dbm: None,
                    rx_to_tx_delay_ns: None,
                    tx_lo_offset_hz: None,
                    num_buffers: Some(radio.bladerf_stream_buffers),
                    buffer_size: Some(radio.bladerf_stream_buffer_samples),
                    num_transfers: Some(radio.bladerf_stream_transfers),
                    stream_timeout_ms: None,
                },
                usize::from(radio.bladerf_rx_sample_phase),
            )
        }
        backend => bail!("unsupported canonical radio backend {backend}"),
    };
    let hal = RadioConfig {
        device,
        tx_rate_policy: RatePolicy::NearestWholeHz,
        rx_rate_policy: RatePolicy::NearestWholeHz,
        tx_sample_rate_hz: umts_types::time::CHIPS_PER_SECOND as usize * tx_samples_per_chip.get(),
        tx_bandwidth_hz: Some(usize::try_from(radio.tx_analog_bandwidth_hz)?),
        rx_sample_rate_hz: Some(SAMPLE_RATE_HZ),
        rx_bandwidth_hz: Some(ANALOG_BANDWIDTH_HZ),
    };
    hal.validate().context("invalid HAL radio configuration")?;
    Ok(RadioPlan {
        hal,
        tx_samples_per_chip,
        downlink_hz,
        uplink_hz,
        rx_phase,
    })
}

#[cfg(test)]
mod tests;
